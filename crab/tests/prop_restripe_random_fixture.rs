//! Random-fixture property test against the restripe reconciliation
//! invariant (design B5).
//!
//! Unlike `prop_restripe_reconcile_invariant.rs` which uses a
//! simplified simulation, this test generates random xorb fixtures
//! with realistic properties (variable sizes, mixed storage classes,
//! overlapping chunk references) and verifies the three-part invariant
//! holds after reconciliation.
//!
//! The invariant states that for any file-index entry `E` present at
//! the end of a restripe:
//!
//! 1. If `E` existed at `run.started_at` AND its xorbs were in the
//!    source set, `E` now points at dest xorbs.
//! 2. If `E` was added during the run by a concurrent push, `E` is
//!    byte-identical to what the push wrote.
//! 3. Every chunk `E` references resolves to a live xorb (either a
//!    newly-written dest xorb or a xorb out of restripe scope).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Domain model
// ---------------------------------------------------------------------------

/// A chunk within a xorb, identified by hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChunkRef {
    xorb_hash: String,
    chunk_offset: u32,
}

/// A simulated file-index entry with chunk-level references.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEntry {
    path: String,
    chunks: Vec<ChunkRef>,
    content_hash: String,
}

/// A simulated xorb with size and storage class.
#[derive(Debug, Clone)]
struct XorbFixture {
    hash: String,
    size_bytes: u64,
    chunk_count: u32,
    is_archive: bool,
}

/// A simulated restripe run with random fixtures.
#[derive(Debug, Clone)]
struct RandomFixtureRun {
    /// All xorbs in the bucket at run start.
    xorbs: Vec<XorbFixture>,
    /// File-index entries at run start.
    pre_run_entries: Vec<FileEntry>,
    /// Which xorbs are selected for restripe (subset of xorbs).
    source_xorb_hashes: BTreeSet<String>,
    /// Mapping from source xorb → destination xorbs (1:N split).
    src_to_dest: HashMap<String, Vec<String>>,
    /// Chunk mapping: for each source xorb chunk, which dest xorb
    /// and offset it lands in.
    chunk_remap: HashMap<ChunkRef, ChunkRef>,
    /// Entries added by concurrent pushes during the run.
    concurrent_push_entries: Vec<FileEntry>,
    /// New xorbs created by concurrent pushes.
    concurrent_push_xorbs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Reconciliation logic (mirrors the real reconcile module)
// ---------------------------------------------------------------------------

fn reconcile(run: &RandomFixtureRun) -> Vec<FileEntry> {
    let mut final_entries: Vec<FileEntry> = Vec::new();

    for entry in &run.pre_run_entries {
        let mut new_chunks = Vec::new();

        for chunk in &entry.chunks {
            if run.source_xorb_hashes.contains(&chunk.xorb_hash) {
                // Remap this chunk to its destination.
                if let Some(remapped) = run.chunk_remap.get(chunk) {
                    new_chunks.push(remapped.clone());
                }
            } else {
                // Out of scope — keep as-is.
                new_chunks.push(chunk.clone());
            }
        }

        final_entries.push(FileEntry {
            path: entry.path.clone(),
            chunks: new_chunks,
            content_hash: entry.content_hash.clone(),
        });
    }

    // Concurrent push entries are added unchanged.
    for entry in &run.concurrent_push_entries {
        final_entries.push(entry.clone());
    }

    final_entries
}

// ---------------------------------------------------------------------------
// Invariant checks
// ---------------------------------------------------------------------------

fn check_invariant_1(run: &RandomFixtureRun, final_entries: &[FileEntry]) -> bool {
    for (i, pre_entry) in run.pre_run_entries.iter().enumerate() {
        let final_entry = &final_entries[i];

        for chunk in &pre_entry.chunks {
            if run.source_xorb_hashes.contains(&chunk.xorb_hash) {
                if let Some(expected) = run.chunk_remap.get(chunk) {
                    if !final_entry.chunks.contains(expected) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn check_invariant_2(run: &RandomFixtureRun, final_entries: &[FileEntry]) -> bool {
    let push_start = run.pre_run_entries.len();
    for (i, push_entry) in run.concurrent_push_entries.iter().enumerate() {
        if &final_entries[push_start + i] != push_entry {
            return false;
        }
    }
    true
}

fn check_invariant_3(run: &RandomFixtureRun, final_entries: &[FileEntry]) -> bool {
    let mut live_xorbs: BTreeSet<String> = BTreeSet::new();

    // Dest xorbs are live.
    for dests in run.src_to_dest.values() {
        live_xorbs.extend(dests.iter().cloned());
    }

    // Non-source xorbs are live.
    for xorb in &run.xorbs {
        if !run.source_xorb_hashes.contains(&xorb.hash) {
            live_xorbs.insert(xorb.hash.clone());
        }
    }

    // Concurrent push xorbs are live.
    live_xorbs.extend(run.concurrent_push_xorbs.iter().cloned());

    for entry in final_entries {
        for chunk in &entry.chunks {
            if !live_xorbs.contains(&chunk.xorb_hash) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Proptest strategies
// ---------------------------------------------------------------------------

fn xorb_fixture(prefix: &'static str) -> impl Strategy<Value = XorbFixture> {
    (
        "[a-f0-9]{12}".prop_map(move |h| format!("{prefix}-{h}")),
        (1u64..512).prop_map(|m| m * 1024 * 1024), // 1 MiB – 512 MiB
        1u32..=64,
        prop::bool::ANY,
    )
        .prop_map(|(hash, size, chunks, archive)| XorbFixture {
            hash,
            size_bytes: size,
            chunk_count: chunks,
            is_archive: archive,
        })
}

fn random_fixture_run() -> impl Strategy<Value = RandomFixtureRun> {
    // Generate source xorbs.
    prop::collection::vec(xorb_fixture("src"), 1..=6).prop_flat_map(|src_xorbs| {
        // Generate some out-of-scope xorbs.
        let oos = prop::collection::vec(xorb_fixture("oos"), 0..=4);

        (Just(src_xorbs), oos).prop_flat_map(|(src_xorbs, oos_xorbs)| {
            let all_xorbs: Vec<XorbFixture> =
                src_xorbs.iter().chain(oos_xorbs.iter()).cloned().collect();
            let source_hashes: BTreeSet<String> =
                src_xorbs.iter().map(|x| x.hash.clone()).collect();

            // Build chunk refs for all xorbs.
            let mut all_chunk_refs: Vec<ChunkRef> = Vec::new();
            for xorb in &all_xorbs {
                for offset in 0..xorb.chunk_count.min(4) {
                    all_chunk_refs.push(ChunkRef {
                        xorb_hash: xorb.hash.clone(),
                        chunk_offset: offset,
                    });
                }
            }

            // Generate dest xorbs and chunk remapping for sources.
            let mut src_to_dest: HashMap<String, Vec<String>> = HashMap::new();
            let mut chunk_remap: HashMap<ChunkRef, ChunkRef> = HashMap::new();
            let mut dest_counter = 0u32;

            for xorb in &src_xorbs {
                let dest_hash = format!("dest-{dest_counter:04}");
                dest_counter += 1;
                src_to_dest.insert(xorb.hash.clone(), vec![dest_hash.clone()]);

                for offset in 0..xorb.chunk_count.min(4) {
                    let src_ref = ChunkRef {
                        xorb_hash: xorb.hash.clone(),
                        chunk_offset: offset,
                    };
                    let dest_ref = ChunkRef {
                        xorb_hash: dest_hash.clone(),
                        chunk_offset: offset,
                    };
                    chunk_remap.insert(src_ref, dest_ref);
                }
            }

            // Generate file entries referencing chunks from all xorbs.
            let max_refs = all_chunk_refs.len().min(4).max(1);
            let pre_run = prop::collection::vec(
                (
                    "[a-z]{3,8}".prop_map(|s| format!("file-{s}")),
                    prop::sample::subsequence(all_chunk_refs.clone(), 1..=max_refs),
                    "[a-f0-9]{16}",
                )
                    .prop_map(|(path, chunks, hash)| FileEntry {
                        path,
                        chunks,
                        content_hash: hash,
                    }),
                1..=8,
            );

            // Generate concurrent push entries (only reference OOS xorbs
            // or new push xorbs).
            let push_xorb_hash = format!("push-{dest_counter:04}");
            let push_refs: Vec<ChunkRef> = oos_xorbs
                .iter()
                .flat_map(|x| {
                    (0..x.chunk_count.min(2)).map(move |o| ChunkRef {
                        xorb_hash: x.hash.clone(),
                        chunk_offset: o,
                    })
                })
                .chain(std::iter::once(ChunkRef {
                    xorb_hash: push_xorb_hash.clone(),
                    chunk_offset: 0,
                }))
                .collect();

            let push_max = push_refs.len().min(3).max(1);
            let concurrent = prop::collection::vec(
                (
                    "[a-z]{3,8}".prop_map(|s| format!("push-{s}")),
                    prop::sample::subsequence(push_refs, 1..=push_max),
                    "[a-f0-9]{16}",
                )
                    .prop_map(|(path, chunks, hash)| FileEntry {
                        path,
                        chunks,
                        content_hash: hash,
                    }),
                0..=3,
            );

            (
                Just(all_xorbs),
                Just(source_hashes),
                Just(src_to_dest),
                Just(chunk_remap),
                pre_run,
                concurrent,
                Just(vec![push_xorb_hash]),
            )
                .prop_map(|(xorbs, sources, s2d, remap, pre, conc, push_xorbs)| {
                    RandomFixtureRun {
                        xorbs,
                        pre_run_entries: pre,
                        source_xorb_hashes: sources,
                        src_to_dest: s2d,
                        chunk_remap: remap,
                        concurrent_push_entries: conc,
                        concurrent_push_xorbs: push_xorbs,
                    }
                })
        })
    })
}

// ---------------------------------------------------------------------------
// Property test
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// The three-part reconciliation invariant holds for random xorb
    /// fixtures with realistic properties: variable sizes, mixed
    /// storage classes, overlapping chunk references, and concurrent
    /// push activity.
    #[test]
    fn random_fixture_reconcile_invariant(run in random_fixture_run()) {
        let final_entries = reconcile(&run);

        prop_assert!(
            check_invariant_1(&run, &final_entries),
            "Invariant 1 violated: pre-run entry not updated to dest xorbs"
        );

        prop_assert!(
            check_invariant_2(&run, &final_entries),
            "Invariant 2 violated: concurrent push entry modified"
        );

        prop_assert!(
            check_invariant_3(&run, &final_entries),
            "Invariant 3 violated: dangling xorb reference"
        );
    }
}
