//! Online reconciliation for concurrent pushes during restripe.
//!
//! At the end of a restripe run, [`finalize`] reads the journal's
//! `src_xorb → dest_xorbs` mapping and reports the work that a metadata
//! reconciliation would need to apply.
//!
//! # Target invariant (design B5)
//!
//! The eventual implementation must prove that, for any file-index entry
//! `E` present at the end of the restripe:
//!
//! 1. If `E` existed at `run.started_at` AND its xorbs were in the
//!    source set, `E` now points at dest xorbs.
//! 2. If `E` was added during the run by a concurrent push, `E` is
//!    byte-identical to what the push wrote.
//! 3. Every chunk `E` references resolves to a live xorb (either a
//!    newly-written dest xorb or a xorb out of restripe scope).
//!
//! Concurrent pushes during the run produce new xorbs outside the
//! restripe snapshot. Those xorbs' file-index entries are untouched by
//! reconciliation. Old source xorbs become orphans and are reclaimed by a
//! later `crab gc` after the metadata commit is complete.
//!
//! A `Store` cannot be used safely yet: the file-index rewrite needs the
//! original `MDBFileInfo` records and a manifest/file-index CAS update, not
//! an advisory object. Apply callers therefore fail closed until that
//! contract is implemented. Tests can pass `None` to inspect journal counts.

use std::collections::HashMap;

use serde::Serialize;
use tracing::{debug, info};

use crate::core::error::{CrabError, Result};
use crate::restripe::journal::{RestripeJournal, SourceStatus};
use crate::storage::store::Store;

// ---------------------------------------------------------------------------
// Reconciliation outcome
// ---------------------------------------------------------------------------

/// Outcome of the reconciliation step.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ReconcileOutcome {
    /// Number of source xorbs that were rewritten to dest xorbs.
    pub entries_updated: u64,
    /// Number of source xorbs that were skipped or corrupt (unchanged).
    pub entries_unchanged: u64,
    /// Number of shards uploaded during reconciliation.
    pub shards_uploaded: u64,
    /// Total bytes uploaded for reconciliation shards.
    pub shard_bytes: u64,
    /// Whether the shard-list CAS succeeded on the first attempt.
    pub cas_first_attempt: bool,
    /// Total CAS attempts needed for the shard-list update.
    pub cas_attempts: u32,
}

// ---------------------------------------------------------------------------
// Source-to-dest mapping
// ---------------------------------------------------------------------------

/// Build the `src_xorb → dest_xorbs` mapping from completed journal entries.
fn build_mapping(
    journal: &RestripeJournal,
    run_id: &str,
) -> Result<(HashMap<String, Vec<String>>, u64, u64)> {
    let done_sources = journal.sources_by_status(run_id, SourceStatus::Done)?;
    let counts = journal.count_by_status(run_id)?;

    let mut src_to_dest: HashMap<String, Vec<String>> = HashMap::new();
    let mut entries_updated: u64 = 0;

    for source in &done_sources {
        if let Some(ref dest_json) = source.dest_xorbs {
            let dests: Vec<String> = serde_json::from_str(dest_json).unwrap_or_default();
            if !dests.is_empty() {
                entries_updated += 1;
                src_to_dest.insert(source.src_xorb.clone(), dests);
            }
        }
    }

    let entries_unchanged = counts.skipped + counts.corrupt;

    Ok((src_to_dest, entries_updated, entries_unchanged))
}

/// Reject xorb apply until the file-index reconciliation contract is real.
pub fn ensure_apply_supported() -> Result<()> {
    Err(CrabError::Configuration {
        key: "optimize xorbs --apply".to_string(),
        origin: "xorb apply is unavailable until it can rewrite MDBFileInfo records and commit the file-index/shard manifest atomically; use --dry-run".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

/// Finalize a restripe run by reconciling the file-index.
///
/// Reads the journal's completed source entries to build the
/// `src_xorb → dest_xorbs` mapping and returns the outcome.
///
/// The reconciliation is scoped to the pre-run xorb snapshot:
/// - Entries whose xorbs were in the source set are updated.
/// - Entries added by concurrent pushes during the run are unchanged.
/// - Every chunk reference in the final file-index resolves to a live
///   xorb (dest xorb or out-of-scope xorb).
pub fn finalize(
    journal: &RestripeJournal,
    run_id: &str,
    store: Option<&Store>,
) -> Result<ReconcileOutcome> {
    // Step 1: Build the src → dest mapping.
    let (src_to_dest, entries_updated, entries_unchanged) = build_mapping(journal, run_id)?;

    debug!(
        updated = entries_updated,
        unchanged = entries_unchanged,
        mappings = src_to_dest.len(),
        "reconciliation mapping built"
    );

    if store.is_some() && !src_to_dest.is_empty() {
        return Err(CrabError::Configuration {
            key: "restripe reconciliation".to_string(),
            origin: "file-index reconciliation is not implemented; refusing to publish destination xorbs that readers cannot resolve".to_string(),
        });
    }

    debug!("no file-index write performed; reporting reconciliation counts only");
    let (shards_uploaded, shard_bytes, cas_first_attempt, cas_attempts) = (0, 0, true, 1);

    info!(
        entries_updated,
        entries_unchanged,
        shards_uploaded,
        shard_bytes,
        cas_attempts,
        "restripe reconciliation complete"
    );

    Ok(ReconcileOutcome {
        entries_updated,
        entries_unchanged,
        shards_uploaded,
        shard_bytes,
        cas_first_attempt,
        cas_attempts,
    })
}

/// Check that a CAS repeat is a no-op (idempotency).
pub fn is_cas_repeat_noop(first_outcome: &ReconcileOutcome) -> bool {
    first_outcome.cas_first_attempt
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cas_repeat_noop_when_first_succeeded() {
        let outcome = ReconcileOutcome {
            entries_updated: 10,
            entries_unchanged: 5,
            shards_uploaded: 1,
            shard_bytes: 256,
            cas_first_attempt: true,
            cas_attempts: 1,
        };
        assert!(is_cas_repeat_noop(&outcome));
    }

    #[test]
    fn cas_repeat_not_noop_when_first_failed() {
        let outcome = ReconcileOutcome {
            entries_updated: 10,
            entries_unchanged: 5,
            shards_uploaded: 1,
            shard_bytes: 256,
            cas_first_attempt: false,
            cas_attempts: 3,
        };
        assert!(!is_cas_repeat_noop(&outcome));
    }

    #[test]
    fn finalize_with_completed_sources_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = RestripeJournal::open(&path).unwrap();

        journal.start_run("test-reconcile", "{}").unwrap();
        journal.insert_source("test-reconcile", "xorb-001").unwrap();
        journal.insert_source("test-reconcile", "xorb-002").unwrap();
        journal.insert_source("test-reconcile", "xorb-003").unwrap();

        journal
            .update_source_status(
                "test-reconcile",
                "xorb-001",
                SourceStatus::Done,
                Some(r#"["dest-001","dest-002"]"#),
            )
            .unwrap();
        journal
            .update_source_status(
                "test-reconcile",
                "xorb-002",
                SourceStatus::Done,
                Some(r#"["dest-003"]"#),
            )
            .unwrap();
        journal
            .update_source_status("test-reconcile", "xorb-003", SourceStatus::Skipped, None)
            .unwrap();

        let outcome = finalize(&journal, "test-reconcile", None).unwrap();

        assert_eq!(outcome.entries_updated, 2);
        assert_eq!(outcome.entries_unchanged, 1);
        assert_eq!(outcome.shards_uploaded, 0); // no store
        assert!(outcome.cas_first_attempt);
    }

    #[test]
    fn finalize_with_empty_dest_lists_counts_zero_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = RestripeJournal::open(&path).unwrap();

        journal.start_run("test-empty", "{}").unwrap();
        journal.insert_source("test-empty", "xorb-aaa").unwrap();
        journal
            .update_source_status("test-empty", "xorb-aaa", SourceStatus::Done, Some("[]"))
            .unwrap();

        let outcome = finalize(&journal, "test-empty", None).unwrap();

        assert_eq!(outcome.entries_updated, 0);
        assert_eq!(outcome.entries_unchanged, 0);
        assert_eq!(outcome.shards_uploaded, 0);
    }

    #[test]
    fn build_mapping_extracts_src_to_dest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = RestripeJournal::open(&path).unwrap();

        journal.start_run("map-test", "{}").unwrap();
        journal.insert_source("map-test", "src-a").unwrap();
        journal.insert_source("map-test", "src-b").unwrap();
        journal.insert_source("map-test", "src-c").unwrap();

        journal
            .update_source_status(
                "map-test",
                "src-a",
                SourceStatus::Done,
                Some(r#"["d1","d2"]"#),
            )
            .unwrap();
        journal
            .update_source_status("map-test", "src-b", SourceStatus::Done, Some("[]"))
            .unwrap();
        journal
            .mark_corrupt("map-test", "src-c", "hash", "bad")
            .unwrap();

        let (mapping, updated, unchanged) = build_mapping(&journal, "map-test").unwrap();

        assert_eq!(mapping.len(), 1); // only src-a has non-empty dests
        assert_eq!(mapping["src-a"], vec!["d1", "d2"]);
        assert_eq!(updated, 1);
        assert_eq!(unchanged, 1); // src-c is corrupt
    }
}
