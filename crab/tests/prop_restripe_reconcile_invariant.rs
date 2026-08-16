//! Property test for the restripe reconciliation invariant (design B5).
//!
//! The three-part invariant states that for any file-index entry `E`
//! present at the end of a restripe:
//!
//! 1. If `E` existed at `run.started_at` AND its xorbs were in the
//!    source set, `E` now points at dest xorbs.
//! 2. If `E` was added during the run by a concurrent push, `E` is
//!    byte-identical to what the push wrote.
//! 3. Every chunk `E` references resolves to a live xorb (either a
//!    newly-written dest xorb or a xorb out of restripe scope).
//!
//! This test models the invariant using an in-memory simulation of the
//! file-index, source/dest xorb mapping, and concurrent push events.

use std::collections::{BTreeSet, HashMap};

use proptest::prelude::*;

/// A simulated file-index entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEntry {
    /// File path.
    path: String,
    /// Xorb hashes this entry references.
    xorb_refs: Vec<String>,
    /// Content hash for byte-identity checks.
    content_hash: String,
}

/// A simulated restripe run.
#[derive(Debug, Clone)]
struct SimulatedRun {
    /// File-index snapshot at run start.
    pre_run_entries: Vec<FileEntry>,
    /// Source xorbs selected for restripe.
    source_xorbs: BTreeSet<String>,
    /// Mapping from source xorb → destination xorbs.
    src_to_dest: HashMap<String, Vec<String>>,
    /// Entries added by concurrent pushes during the run.
    concurrent_push_entries: Vec<FileEntry>,
}

/// Apply the reconciliation logic and return the final file-index.
fn reconcile(run: &SimulatedRun) -> Vec<FileEntry> {
    let mut final_entries: Vec<FileEntry> = Vec::new();

    // Process pre-run entries.
    for entry in &run.pre_run_entries {
        let mut new_refs = Vec::new();

        for xorb in &entry.xorb_refs {
            if run.source_xorbs.contains(xorb) {
                // This xorb was restriped — replace with dest xorbs.
                if let Some(dests) = run.src_to_dest.get(xorb) {
                    new_refs.extend(dests.iter().cloned());
                }
            } else {
                // Out of scope — keep as-is.
                new_refs.push(xorb.clone());
            }
        }

        final_entries.push(FileEntry {
            path: entry.path.clone(),
            xorb_refs: new_refs,
            content_hash: entry.content_hash.clone(),
        });
    }

    // Concurrent push entries are added unchanged.
    for entry in &run.concurrent_push_entries {
        final_entries.push(entry.clone());
    }

    final_entries
}

/// Check invariant 1: pre-run entries with source xorbs now point at dest xorbs.
fn check_invariant_1(run: &SimulatedRun, final_entries: &[FileEntry]) -> bool {
    for (i, pre_entry) in run.pre_run_entries.iter().enumerate() {
        let final_entry = &final_entries[i];

        for xorb in &pre_entry.xorb_refs {
            if run.source_xorbs.contains(xorb) {
                // This xorb should have been replaced by dest xorbs.
                if let Some(dests) = run.src_to_dest.get(xorb) {
                    for dest in dests {
                        if !final_entry.xorb_refs.contains(dest) {
                            return false;
                        }
                    }
                }
            }
        }
    }
    true
}

/// Check invariant 2: concurrent push entries are byte-identical.
fn check_invariant_2(run: &SimulatedRun, final_entries: &[FileEntry]) -> bool {
    let push_start = run.pre_run_entries.len();
    for (i, push_entry) in run.concurrent_push_entries.iter().enumerate() {
        let final_entry = &final_entries[push_start + i];
        if final_entry != push_entry {
            return false;
        }
    }
    true
}

/// Check invariant 3: every xorb ref resolves to a live xorb.
fn check_invariant_3(run: &SimulatedRun, final_entries: &[FileEntry]) -> bool {
    // Live xorbs = dest xorbs + xorbs not in source set.
    let mut live_xorbs: BTreeSet<String> = BTreeSet::new();

    // All dest xorbs are live.
    for dests in run.src_to_dest.values() {
        live_xorbs.extend(dests.iter().cloned());
    }

    // All xorbs not in the source set are live (out of scope).
    for entry in &run.pre_run_entries {
        for xorb in &entry.xorb_refs {
            if !run.source_xorbs.contains(xorb) {
                live_xorbs.insert(xorb.clone());
            }
        }
    }

    // All xorbs from concurrent pushes are live.
    for entry in &run.concurrent_push_entries {
        for xorb in &entry.xorb_refs {
            live_xorbs.insert(xorb.clone());
        }
    }

    // Check every ref in final entries resolves.
    for entry in final_entries {
        for xorb in &entry.xorb_refs {
            if !live_xorbs.contains(xorb) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Proptest strategies
// ---------------------------------------------------------------------------

fn xorb_hash() -> impl Strategy<Value = String> {
    "[a-f0-9]{8}".prop_map(|s| format!("xorb-{s}"))
}

fn dest_hash() -> impl Strategy<Value = String> {
    "[a-f0-9]{8}".prop_map(|s| format!("dest-{s}"))
}

fn file_entry_strategy(available_xorbs: Vec<String>) -> impl Strategy<Value = FileEntry> {
    let xorbs = available_xorbs.clone();
    let max_refs = xorbs.len().min(3).max(1);
    (
        "[a-z]{3,8}".prop_map(|s| format!("file-{s}")),
        prop::sample::subsequence(xorbs, 1..=max_refs),
        "[a-f0-9]{16}",
    )
        .prop_map(|(path, refs, hash)| FileEntry {
            path,
            xorb_refs: refs,
            content_hash: hash,
        })
}

fn simulated_run_strategy() -> impl Strategy<Value = SimulatedRun> {
    // Generate source xorbs.
    prop::collection::vec(xorb_hash(), 1..=8).prop_flat_map(|source_xorbs| {
        let sources: BTreeSet<String> = source_xorbs.iter().cloned().collect();
        let source_list = source_xorbs.clone();

        // Generate dest mappings for each source.
        let dest_mappings = source_xorbs
            .iter()
            .map(|src| {
                let src = src.clone();
                prop::collection::vec(dest_hash(), 1..=3)
                    .prop_map(move |dests| (src.clone(), dests))
            })
            .collect::<Vec<_>>();

        // Generate some out-of-scope xorbs.
        let out_of_scope = prop::collection::vec(xorb_hash(), 0..=4);

        (dest_mappings, out_of_scope).prop_flat_map(move |(dest_strats, oos_xorbs)| {
            let sources = sources.clone();
            let source_list = source_list.clone();

            // Collect all available xorbs for file entries.
            let mut all_xorbs: Vec<String> = source_list.clone();
            all_xorbs.extend(oos_xorbs.iter().cloned());

            // Generate pre-run file entries.
            let pre_run = prop::collection::vec(file_entry_strategy(all_xorbs.clone()), 1..=6);

            // Generate concurrent push entries (use only out-of-scope xorbs).
            let push_xorbs: Vec<String> = oos_xorbs
                .iter()
                .chain(std::iter::once(&"push-new-xorb".to_string()))
                .cloned()
                .collect();
            let concurrent = prop::collection::vec(file_entry_strategy(push_xorbs), 0..=3);

            (Just(sources), Just(dest_strats), pre_run, concurrent).prop_map(
                |(sources, dest_pairs, pre_run, concurrent)| {
                    let src_to_dest: HashMap<String, Vec<String>> =
                        dest_pairs.into_iter().collect();
                    SimulatedRun {
                        pre_run_entries: pre_run,
                        source_xorbs: sources,
                        src_to_dest,
                        concurrent_push_entries: concurrent,
                    }
                },
            )
        })
    })
}

// ---------------------------------------------------------------------------
// Property test
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// The three-part reconciliation invariant holds for any combination
    /// of pre-run entries, source/dest mappings, and concurrent pushes.
    #[test]
    fn prop_restripe_reconcile_invariant(run in simulated_run_strategy()) {
        let final_entries = reconcile(&run);

        // Invariant 1: pre-run entries with source xorbs point at dest xorbs.
        prop_assert!(
            check_invariant_1(&run, &final_entries),
            "Invariant 1 violated: pre-run entry not updated to dest xorbs"
        );

        // Invariant 2: concurrent push entries are byte-identical.
        prop_assert!(
            check_invariant_2(&run, &final_entries),
            "Invariant 2 violated: concurrent push entry modified"
        );

        // Invariant 3: every xorb ref resolves to a live xorb.
        prop_assert!(
            check_invariant_3(&run, &final_entries),
            "Invariant 3 violated: dangling xorb reference"
        );
    }
}
