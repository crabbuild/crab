//! Bounded, crash-safe source-xorb to destination-xorb rewrite pipeline.
//!
//! Source rows are consumed in deterministic pages. A page shares one
//! bounded builder, so fragments from multiple source xorbs are consolidated
//! without loading the entire journal into memory. Immutable destinations are
//! uploaded before any source row is committed as done.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::optimize::xorbs::journal::{OptimizeXorbsJournal, SourceRow, SourceStatus};
use crate::optimize::xorbs::profile::Profile;
use crate::storage::head_class::head_with_class;
use crate::storage::store::Store;
use crate::tier::restore::RestoreOrchestrator;
use crab_storage::canonical_global_content_path;
use crab_xet::xorb::builder::{FixedCompression, RunId, XorbBuilder, XorbResult};
use crab_xet::xorb::format::{CompressionScheme, MAX_XORB_SIZE, MerkleHash};
use crab_xet::xorb::parser::XorbParser;

const SOURCES_PER_OPTIMIZE_BATCH: usize = 64;
const MIN_TARGET_XORB_BYTES: usize = 4 * 1024 * 1024;
const MAX_TARGET_XORB_BYTES: usize = 256 * 1024 * 1024;
const MAX_SOURCE_XORB_BYTES: u64 = MAX_XORB_SIZE as u64;
const MAX_CORRUPT_REPORT_ENTRIES: usize = 1_024;

/// Configuration for the xorb optimization executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub include_cold: bool,
    pub restore_tier: String,
    pub output_class: String,
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

/// Per-xorb progress event emitted during execution.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct XorbProgressEvent {
    pub src_xorb: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest_xorbs: Option<Vec<String>>,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub elapsed_ms: u64,
}

/// Summary of a completed xorb optimization.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExecutorOutcome {
    pub run_id: String,
    pub profile: String,
    pub sources_processed: u64,
    pub sources_done: u64,
    pub sources_corrupt: u64,
    pub sources_skipped: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub elapsed_ms: u64,
    pub corrupt_list: Vec<String>,
    pub corrupt_list_omitted: u64,
}

/// Execute one bounded xorb optimization run.
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
    let pending = journal.count_by_status(run_id)?.pending;
    if pending == 0 {
        return Ok(build_outcome(
            run_id,
            profile,
            &start,
            BatchResult::default(),
        ));
    }

    let compression = resolve_compression(profile)?;
    validate_target(profile)?;
    info!(pending, "processing live source xorbs");

    let mut total = BatchResult::default();
    let mut after = String::new();
    loop {
        check_cancelled(cancel)?;
        let rows = journal.sources_by_status_after(
            run_id,
            SourceStatus::Pending,
            Some(&after),
            SOURCES_PER_OPTIMIZE_BATCH,
        )?;
        if rows.is_empty() {
            break;
        }
        let last = rows.last().map(|row| row.src_xorb.clone()).ok_or_else(|| {
            CrabError::Internal("pending journal page unexpectedly empty".to_owned())
        })?;
        let result = match store {
            Some(store) => {
                process_batch(
                    journal,
                    run_id,
                    &rows,
                    profile,
                    config,
                    compression,
                    store,
                    restore_orchestrator,
                    cancel,
                )
                .await?
            }
            None => mark_batch_without_store(journal, run_id, &rows, cancel)?,
        };
        total.merge(result);
        after = last;
    }

    info!(
        done = total.done,
        corrupt = total.corrupt.len() as u64 + total.corrupt_omitted,
        skipped = total.skipped,
        bytes_read = total.bytes_read,
        bytes_written = total.bytes_written,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "xorb rewrite pipeline complete"
    );
    Ok(build_outcome(run_id, profile, &start, total))
}

#[derive(Default)]
struct BatchResult {
    processed: u64,
    done: u64,
    skipped: u64,
    corrupt: Vec<String>,
    corrupt_omitted: u64,
    bytes_read: u64,
    bytes_written: u64,
}

impl BatchResult {
    fn merge(&mut self, other: Self) {
        self.processed = self.processed.saturating_add(other.processed);
        self.done = self.done.saturating_add(other.done);
        self.skipped = self.skipped.saturating_add(other.skipped);
        self.bytes_read = self.bytes_read.saturating_add(other.bytes_read);
        self.bytes_written = self.bytes_written.saturating_add(other.bytes_written);
        self.corrupt.extend(other.corrupt);
        if self.corrupt.len() > MAX_CORRUPT_REPORT_ENTRIES {
            let overflow = self.corrupt.len() - MAX_CORRUPT_REPORT_ENTRIES;
            self.corrupt.truncate(MAX_CORRUPT_REPORT_ENTRIES);
            self.corrupt_omitted = self.corrupt_omitted.saturating_add(overflow as u64);
        }
        self.corrupt_omitted = self.corrupt_omitted.saturating_add(other.corrupt_omitted);
    }

    fn record_corrupt(&mut self, hash: String) {
        if self.corrupt.len() < MAX_CORRUPT_REPORT_ENTRIES {
            self.corrupt.push(hash);
        } else {
            self.corrupt_omitted = self.corrupt_omitted.saturating_add(1);
        }
    }
}

fn mark_batch_without_store(
    journal: &OptimizeXorbsJournal,
    run_id: &str,
    rows: &[SourceRow],
    cancel: &CancellationToken,
) -> Result<BatchResult> {
    let mut result = BatchResult::default();
    for row in rows {
        check_cancelled(cancel)?;
        journal.update_source_status(run_id, &row.src_xorb, SourceStatus::Done, Some("[]"))?;
        result.processed = result.processed.saturating_add(1);
        result.done = result.done.saturating_add(1);
    }
    Ok(result)
}

#[derive(Debug)]
struct PreparedSource {
    hash: String,
    chunks: Vec<MerkleHash>,
}

#[expect(clippy::too_many_arguments, reason = "bounded pipeline context")]
async fn process_batch(
    journal: &OptimizeXorbsJournal,
    run_id: &str,
    source_rows: &[SourceRow],
    profile: &Profile,
    config: &ExecutorConfig,
    compression: CompressionScheme,
    store: &Store,
    restore_orchestrator: Option<&RestoreOrchestrator>,
    cancel: &CancellationToken,
) -> Result<BatchResult> {
    let policy = Arc::new(FixedCompression::new(compression));
    let target =
        usize::try_from(profile.target_xorb_bytes).map_err(|_| CrabError::Configuration {
            key: "optimize xorbs target size".to_owned(),
            origin: "target size cannot be represented on this platform".to_owned(),
        })?;
    let mut builder =
        XorbBuilder::with_policy(policy as Arc<dyn crab_xet::xorb::builder::CompressionPolicy>)
            .with_size_bounds(MIN_TARGET_XORB_BYTES, MAX_TARGET_XORB_BYTES);
    builder.set_target_size(target);

    let memory_budget = profile
        .target_xorb_bytes
        .saturating_mul(config.budget_factor);
    let mut outcome = BatchResult::default();
    let mut prepared = Vec::with_capacity(source_rows.len());
    let mut placements = HashMap::<MerkleHash, String>::new();

    for (source_index, source_row) in source_rows.iter().enumerate() {
        check_cancelled(cancel)?;
        let source_hash = &source_row.src_xorb;
        let source_path = canonical_global_content_path("xorbs", source_hash);
        outcome.processed = outcome.processed.saturating_add(1);

        let object = tokio::select! {
            result = store.head(&source_path) => result,
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
        }
        .map_err(|error| match error {
            CrabError::NotFound { .. } => CrabError::CorruptObject {
                path: source_path.to_string(),
                reason: "journal references a missing source xorb".to_owned(),
            },
            error => error,
        })?;
        if object.size > MAX_SOURCE_XORB_BYTES {
            return Err(CrabError::Configuration {
                key: "optimize xorbs source size".to_owned(),
                origin: format!(
                    "source {source_hash} is {} bytes; bounded rewriting supports at most {MAX_SOURCE_XORB_BYTES} bytes",
                    object.size
                ),
            });
        }

        let head = tokio::select! {
            result = head_with_class(store, &source_path) => result?,
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
        };
        if head.class.is_archive_class() {
            if !config.include_cold {
                journal.update_source_status(run_id, source_hash, SourceStatus::Skipped, None)?;
                outcome.skipped = outcome.skipped.saturating_add(1);
                continue;
            }
            let orchestrator =
                restore_orchestrator.ok_or_else(|| CrabError::ArchiveRestoreRequired {
                    xorb: source_path.to_string(),
                    class: head.class.to_string(),
                    estimated_eta: None,
                })?;
            orchestrator.ensure_warm(&source_path.to_string()).await?;
            check_cancelled(cancel)?;
        }

        let (source_bytes, _) = tokio::select! {
            result = store.get_with_etag_bounded(&source_path, MAX_SOURCE_XORB_BYTES) => result?,
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
        };
        let source_size =
            u64::try_from(source_bytes.len()).map_err(|_| CrabError::Configuration {
                key: "optimize xorbs source size".to_owned(),
                origin: "source size cannot be represented".to_owned(),
            })?;
        if source_size > memory_budget {
            journal.update_source_status(run_id, source_hash, SourceStatus::Skipped, None)?;
            outcome.skipped = outcome.skipped.saturating_add(1);
            outcome.bytes_read = outcome.bytes_read.saturating_add(source_size);
            continue;
        }
        if source_size != object.size {
            return Err(CrabError::CorruptObject {
                path: source_path.to_string(),
                reason: format!(
                    "source xorb size changed during read (HEAD reported {}, GET returned {source_size})",
                    object.size
                ),
            });
        }
        outcome.bytes_read = outcome.bytes_read.saturating_add(source_size);

        let parser = match XorbParser::parse(source_bytes) {
            Ok(parser) => parser,
            Err(error) => {
                journal.mark_corrupt(run_id, source_hash, "parse_failed", &error.to_string())?;
                outcome.record_corrupt(source_hash.clone());
                continue;
            }
        };
        if parser.hash().hex() != *source_hash {
            let actual = parser.hash().hex();
            journal.mark_corrupt(
                run_id,
                source_hash,
                "hash_mismatch",
                &format!("expected {source_hash}, got {actual}"),
            )?;
            outcome.record_corrupt(source_hash.clone());
            continue;
        }
        if let Err(error) = parser.verify_payload_digest() {
            journal.mark_corrupt(run_id, source_hash, "payload_digest", &error.to_string())?;
            outcome.record_corrupt(source_hash.clone());
            continue;
        }
        if let Err(error) = parser.verify_all_chunks() {
            journal.mark_corrupt(
                run_id,
                source_hash,
                "chunk_verification",
                &error.to_string(),
            )?;
            outcome.record_corrupt(source_hash.clone());
            continue;
        }

        let source_run = RunId(u64::try_from(source_index).map_err(|_| {
            CrabError::Internal("source batch index cannot be represented".to_owned())
        })?);
        let mut chunks = Vec::with_capacity(parser.num_chunks() as usize);
        for chunk_index in 0..parser.num_chunks() {
            check_cancelled(cancel)?;
            let chunk = parser.get_chunk(chunk_index)?;
            chunks.push(chunk.hash);
            let _ = builder.push(&chunk, source_run)?;
            while let Some(destination) = builder.take_completed() {
                outcome.bytes_written = outcome.bytes_written.saturating_add(
                    upload_destination(store, destination, &mut placements, cancel).await?,
                );
            }
        }
        prepared.push(PreparedSource {
            hash: source_hash.clone(),
            chunks,
        });
    }

    for destination in builder.finalize()? {
        outcome.bytes_written = outcome
            .bytes_written
            .saturating_add(upload_destination(store, destination, &mut placements, cancel).await?);
    }
    check_cancelled(cancel)?;
    for source in prepared {
        check_cancelled(cancel)?;
        let mut destinations = source
            .chunks
            .iter()
            .map(|chunk| {
                placements.get(chunk).cloned().ok_or_else(|| {
                    CrabError::Internal(format!(
                        "source {} chunk {} has no destination placement",
                        source.hash, chunk
                    ))
                })
            })
            .collect::<Result<HashSet<_>>>()?
            .into_iter()
            .collect::<Vec<_>>();
        destinations.sort_unstable();
        let encoded = serde_json::to_string(&destinations).map_err(|error| {
            CrabError::Internal(format!("destination list serialize failed: {error}"))
        })?;
        journal.update_source_status(run_id, &source.hash, SourceStatus::Done, Some(&encoded))?;
        outcome.done = outcome.done.saturating_add(1);
    }
    Ok(outcome)
}

async fn upload_destination(
    store: &Store,
    destination: XorbResult,
    placements: &mut HashMap<MerkleHash, String>,
    cancel: &CancellationToken,
) -> Result<u64> {
    let destination_hash = destination.hash;
    let hash = destination_hash.hex();
    let path = canonical_global_content_path("xorbs", &hash);
    let size = u64::try_from(destination.bytes.len()).map_err(|_| CrabError::Configuration {
        key: "optimize xorbs destination size".to_owned(),
        origin: format!("destination xorb {hash} size cannot be represented"),
    })?;
    if size > MAX_TARGET_XORB_BYTES as u64 {
        return Err(CrabError::Configuration {
            key: "optimize xorbs destination size".to_owned(),
            origin: format!(
                "destination xorb {hash} is {size} bytes; bounded rewriting supports at most {MAX_TARGET_XORB_BYTES} bytes"
            ),
        });
    }

    match store
        .put_multipart_retry_with_xet_hash(
            &path,
            Bytes::from(destination.bytes.clone()),
            destination_hash.into(),
            8 * 1024 * 1024,
            cancel,
            None,
        )
        .await
    {
        Ok(()) => {}
        Err(CrabError::CasConflict { .. }) => {
            let (existing, _) = store
                .get_with_etag_bounded(&path, MAX_TARGET_XORB_BYTES as u64)
                .await?;
            let parser = XorbParser::parse(existing).map_err(CrabError::from)?;
            if parser.hash() != destination_hash {
                return Err(CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!("existing xorb hashes to {}, expected {hash}", parser.hash()),
                });
            }
            parser.verify_payload_digest().map_err(CrabError::from)?;
            parser.verify_all_chunks().map_err(CrabError::from)?;
        }
        Err(error) => return Err(error),
    }

    for placement in destination.placements {
        if let Some(previous) = placements.insert(placement.chunk_hash, hash.clone())
            && previous != hash
        {
            return Err(CrabError::Internal(format!(
                "chunk {} was packed into both {previous} and {hash}",
                placement.chunk_hash
            )));
        }
    }
    Ok(size)
}

fn validate_target(profile: &Profile) -> Result<()> {
    let target =
        usize::try_from(profile.target_xorb_bytes).map_err(|_| CrabError::Configuration {
            key: "optimize xorbs target size".to_owned(),
            origin: "target size cannot be represented on this platform".to_owned(),
        })?;
    if !(MIN_TARGET_XORB_BYTES..=MAX_TARGET_XORB_BYTES).contains(&target) {
        return Err(CrabError::Configuration {
            key: "optimize xorbs target size".to_owned(),
            origin: format!(
                "target must be between {MIN_TARGET_XORB_BYTES} and {MAX_TARGET_XORB_BYTES} bytes"
            ),
        });
    }
    Ok(())
}

fn resolve_compression(profile: &Profile) -> Result<CompressionScheme> {
    use crate::core::config::CompressionConfig;
    match profile.compression {
        CompressionConfig::None => Ok(CompressionScheme::None),
        CompressionConfig::Lz4 | CompressionConfig::Zstd { .. } => Ok(CompressionScheme::LZ4),
    }
}

fn build_outcome(
    run_id: &str,
    profile: &Profile,
    start: &Instant,
    result: BatchResult,
) -> ExecutorOutcome {
    ExecutorOutcome {
        run_id: run_id.to_owned(),
        profile: profile.to_json(),
        sources_processed: result.processed,
        sources_done: result.done,
        sources_corrupt: (result.corrupt.len() as u64).saturating_add(result.corrupt_omitted),
        sources_skipped: result.skipped,
        bytes_read: result.bytes_read,
        bytes_written: result.bytes_written,
        elapsed_ms: start.elapsed().as_millis() as u64,
        corrupt_list: result.corrupt,
        corrupt_list_omitted: result.corrupt_omitted,
    }
}

/// Reject local GC implementations that do not participate in the remote lease.
pub fn check_gc_not_running(crab_dir: &std::path::Path) -> Result<()> {
    if crab_dir.join("gc.lock").exists() {
        return Err(CrabError::ConcurrentMaintenance { other: "gc" });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn executor_config_defaults() {
        let config = ExecutorConfig::default();
        assert!(config.include_cold);
        assert_eq!(config.restore_tier, "standard");
        assert_eq!(config.output_class, "STANDARD");
        assert_eq!(config.budget_factor, 2);
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
    fn zstd_profile_uses_xet_lz4_scheme() {
        let mut profile = Profile::code();
        profile.compression = crate::core::config::CompressionConfig::Zstd { level: 3 };
        assert_eq!(
            resolve_compression(&profile).unwrap(),
            CompressionScheme::LZ4
        );
    }

    #[tokio::test]
    async fn execute_with_no_store_marks_pending_as_done() {
        let dir = tempfile::tempdir().unwrap();
        let journal = OptimizeXorbsJournal::open(&dir.path().join("journal.db")).unwrap();
        journal.start_run("run", "{}").unwrap();
        journal.insert_source("run", "xorb-a").unwrap();
        journal.insert_source("run", "xorb-b").unwrap();
        let outcome = execute(
            &journal,
            "run",
            &Profile::code(),
            &ExecutorConfig::default(),
            &CancellationToken::new(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.sources_done, 2);
        assert_eq!(journal.count_by_status("run").unwrap().pending, 0);
    }

    #[tokio::test]
    async fn execute_cancellation_stops_before_first_page() {
        let dir = tempfile::tempdir().unwrap();
        let journal = OptimizeXorbsJournal::open(&dir.path().join("journal.db")).unwrap();
        journal.start_run("run", "{}").unwrap();
        journal.insert_source("run", "xorb-a").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = execute(
            &journal,
            "run",
            &Profile::code(),
            &ExecutorConfig::default(),
            &cancel,
            None,
            None,
        )
        .await;
        assert!(matches!(result, Err(CrabError::Cancelled)));
        assert_eq!(journal.count_by_status("run").unwrap().pending, 1);
    }
}
