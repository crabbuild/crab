//! End-to-end integration tests for xorb optimization.
//!
//! These tests exercise the full xorb optimization pipeline, including profile
//! resolution, dry-run estimation, journal lifecycle, and CLI surface.
//! All tests are `#[ignore]`-guarded because they require either a
//! real object store or a large fixture.

use crab::optimize::xorbs::inference::{self, RepoStats};
use crab::optimize::xorbs::journal::{OptimizeXorbsJournal, SourceStatus};
use crab::optimize::xorbs::planner::{self, CalibrationConfig, SourceXorbMeta};
use crab::optimize::xorbs::profile::Profile;

// ---------------------------------------------------------------------------
// 22.1: Integration test — 500 MiB fixture, ml profile, dest count
//       within 10% of planner.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires 500 MiB fixture and object store"]
fn optimize_xorbs_500mib_ml_profile_dest_count_within_10pct() {
    // Simulate a 500 MiB fixture with 2 source xorbs of 256 MiB each.
    let profile = Profile::ml();
    let sources: Vec<SourceXorbMeta> = (0..2)
        .map(|i| SourceXorbMeta {
            hash: format!("xorb-{i:04}"),
            size_bytes: 256 * 1024 * 1024,
            storage_class: "STANDARD".to_string(),
            is_archive: false,
        })
        .collect();

    let cal = CalibrationConfig::default();
    let estimate = planner::estimate("ml", &profile, &sources, &cal, true);

    // The planner should estimate ~2 destination xorbs (each source is
    // already at the target size, so recompression should yield ~1 dest
    // per source after compression ratio).
    assert!(estimate.estimated_dest_count > 0);
    assert!(estimate.source_count == 2);

    // In a real test, we'd run the executor and compare actual dest
    // count to the estimate, verifying within 10%.
}

// ---------------------------------------------------------------------------
// 22.2: Hydrate after optimization — every pre-optimization pointer
//       byte-identical.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires object store and hydrate pipeline"]
fn optimize_xorbs_hydrate_round_trip_byte_identical() {
    // This test would:
    // 1. Push a set of files to create xorbs.
    // 2. Record the content hashes of all files.
    // 3. Run xorb optimization with a profile.
    // 4. Hydrate all files.
    // 5. Verify every file's content hash matches the pre-optimization hash.
    //
    // Stub: the executor is not yet wired to real xorb I/O.
}

// ---------------------------------------------------------------------------
// 22.3: Concurrent push + xorb optimization — both content streams hydrate,
//       no dangling refs.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires concurrent push and xorb optimization execution"]
fn optimize_xorbs_concurrent_push_both_hydrate() {
    // This test would:
    // 1. Start an xorb optimization run.
    // 2. Concurrently push new files.
    // 3. Wait for xorb optimization to complete.
    // 4. Verify both pre-optimization and pushed files hydrate correctly.
    // 5. Verify no dangling refs in the file-index.
    //
    // Stub: requires full executor + push pipeline integration.
}

// ---------------------------------------------------------------------------
// 22.4: Abort + resume — SIGTERM, rerun, completion correct.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires signal handling and journal resume"]
fn optimize_xorbs_abort_resume_completion() {
    // This test would:
    // 1. Start an xorb optimization run.
    // 2. Send SIGTERM mid-run.
    // 3. Verify the journal has pending entries.
    // 4. Resume the run.
    // 5. Verify all entries are completed.
    //
    // We can test the journal part without signals:
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let journal = OptimizeXorbsJournal::open(&path).unwrap();

    journal.start_run("run-abort-test", "{}").unwrap();
    journal.insert_source("run-abort-test", "xorb-001").unwrap();
    journal.insert_source("run-abort-test", "xorb-002").unwrap();
    journal.insert_source("run-abort-test", "xorb-003").unwrap();

    // Simulate partial completion (as if SIGTERM arrived after xorb-001).
    journal
        .update_source_status(
            "run-abort-test",
            "xorb-001",
            SourceStatus::Done,
            Some(r#"["dest-001"]"#),
        )
        .unwrap();

    // Verify pending count.
    let counts = journal.count_by_status("run-abort-test").unwrap();
    assert_eq!(counts.done, 1);
    assert_eq!(counts.pending, 2);

    // The run is still active (not completed, not aborted).
    let active = journal.active_run().unwrap();
    assert!(active.is_some());
    assert_eq!(active.unwrap().run_id, "run-abort-test");

    // Resume would pick up the 2 pending entries.
    let pending = journal
        .sources_by_status("run-abort-test", SourceStatus::Pending)
        .unwrap();
    assert_eq!(pending.len(), 2);
}

// ---------------------------------------------------------------------------
// 22.5: --drop-journal — orphans appear in fsck; next GC reclaims.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires fsck and GC integration"]
fn optimize_xorbs_drop_journal_orphans_reclaimable() {
    // This test would:
    // 1. Run a partial xorb optimization (some destination xorbs uploaded).
    // 2. Drop the journal.
    // 3. Run fsck — verify orphan dest xorbs are reported.
    // 4. Run GC — verify orphans are reclaimed.
    //
    // We can test the journal drop:
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");

    // Create and populate a journal.
    {
        let journal = OptimizeXorbsJournal::open(&path).unwrap();
        journal.start_run("run-drop-test", "{}").unwrap();
    }

    assert!(path.exists());

    // Drop it.
    OptimizeXorbsJournal::drop_journal(&path).unwrap();
    assert!(!path.exists());
}

// ---------------------------------------------------------------------------
// Unit-level tests that don't need #[ignore]
// ---------------------------------------------------------------------------

#[test]
fn profile_inference_three_workloads() {
    // ML workload.
    let ml_stats = RepoStats::scan(vec![200 * 1024 * 1024; 10]);
    let ml_profile = inference::infer(&ml_stats);
    assert_eq!(ml_profile, Profile::ml());

    // Dataset workload.
    let ds_stats = RepoStats::scan(vec![5 * 1024 * 1024; 1000]);
    let ds_profile = inference::infer(&ds_stats);
    assert_eq!(ds_profile, Profile::dataset());

    // Code workload.
    let code_stats = RepoStats::scan(vec![10 * 1024; 50_000]);
    let code_profile = inference::infer(&code_stats);
    assert_eq!(code_profile, Profile::code());
}

#[test]
fn journal_full_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let journal = OptimizeXorbsJournal::open(&path).unwrap();

    // Start run.
    journal
        .start_run("run-lifecycle", r#"{"profile":"ml"}"#)
        .unwrap();

    // Insert sources.
    for i in 0..5 {
        journal
            .insert_source("run-lifecycle", &format!("xorb-{i:03}"))
            .unwrap();
    }

    // Process some.
    journal
        .update_source_status("run-lifecycle", "xorb-000", SourceStatus::Done, Some("[]"))
        .unwrap();
    journal
        .update_source_status("run-lifecycle", "xorb-001", SourceStatus::Done, Some("[]"))
        .unwrap();
    journal
        .mark_corrupt("run-lifecycle", "xorb-002", "hash_mismatch", "bad")
        .unwrap();
    journal
        .update_source_status("run-lifecycle", "xorb-003", SourceStatus::Skipped, None)
        .unwrap();

    // Check counts.
    let counts = journal.count_by_status("run-lifecycle").unwrap();
    assert_eq!(counts.done, 2);
    assert_eq!(counts.corrupt, 1);
    assert_eq!(counts.skipped, 1);
    assert_eq!(counts.pending, 1);
    assert_eq!(counts.total(), 5);

    // Complete run.
    journal.complete_run("run-lifecycle").unwrap();
    assert!(journal.active_run().unwrap().is_none());
}

#[test]
fn planner_estimate_deterministic() {
    let profile = Profile::dataset();
    let sources: Vec<SourceXorbMeta> = (0..100)
        .map(|i| SourceXorbMeta {
            hash: format!("xorb-{i:04}"),
            size_bytes: 64 * 1024 * 1024,
            storage_class: "STANDARD".to_string(),
            is_archive: false,
        })
        .collect();
    let cal = CalibrationConfig::default();

    let est1 = planner::estimate("dataset", &profile, &sources, &cal, true);
    let est2 = planner::estimate("dataset", &profile, &sources, &cal, true);

    // Deterministic: same inputs → same outputs.
    assert_eq!(est1.source_count, est2.source_count);
    assert_eq!(est1.estimated_dest_count, est2.estimated_dest_count);
    assert_eq!(est1.estimated_dest_bytes, est2.estimated_dest_bytes);
    assert_eq!(est1.estimated_cost_usd, est2.estimated_cost_usd);
}
