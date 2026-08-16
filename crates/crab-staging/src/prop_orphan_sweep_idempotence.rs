//! Orphan sweep idempotence: two consecutive `sweep_orphans` calls
//! produce `segments_removed == 0` on the second.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;

use crate::index::{Index, PendingRow};
use crate::segment::SegmentWriter;
use proptest::prelude::*;

/// Deterministic chunk hash from segment index and chunk index.
fn test_chunk_hash(seg_idx: usize, chunk_idx: usize) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&(seg_idx as u64).to_le_bytes());
    h[8..16].copy_from_slice(&(chunk_idx as u64).to_le_bytes());
    h
}

/// Deterministic file hash from a segment index.
fn test_file_hash(seg_idx: usize) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[0] = 0xF0;
    h[1..9].copy_from_slice(&(seg_idx as u64).to_le_bytes());
    h
}

/// A test scenario: a list of segments, each with a set of chunks, and
/// a subset of segments that are live (registered to a file). Non-live
/// segments are explicitly retired before sweep.
#[derive(Debug, Clone)]
struct SweepScenario {
    /// Each segment is a vec of chunk data blobs.
    segments: Vec<Vec<Vec<u8>>>,
    /// Indices of segments whose chunks should be registered as live.
    /// These segments should survive the sweep.
    live_segment_indices: Vec<usize>,
}

fn sweep_scenario_strategy() -> impl Strategy<Value = SweepScenario> {
    // 1–5 segments, each with 1–10 chunks of 8–128 bytes.
    let segments = prop::collection::vec(
        prop::collection::vec(
            prop::collection::vec(any::<u8>(), 8..=128usize),
            1..=10usize,
        ),
        1..=5usize,
    );

    segments.prop_flat_map(|segs| {
        let n = segs.len();
        // Pick a random subset of segment indices to be "live".
        let live = prop::collection::vec(0..n, 0..=n);
        live.prop_map(move |indices| {
            // Deduplicate indices.
            let mut unique: Vec<usize> = indices.into_iter().collect();
            unique.sort_unstable();
            unique.dedup();
            SweepScenario {
                segments: segs.clone(),
                live_segment_indices: unique,
            }
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(15))]

    #[test]
    fn orphan_sweep_is_idempotent(scenario in sweep_scenario_strategy()) {
        run_sweep_idempotence_test(&scenario)?;
    }
}

/// Helper that mimics `StagingArea::sweep_orphans` logic at the Index level,
/// operating on a tempdir-backed staging area.
fn do_sweep(
    index: &Index,
    segments_dir: &std::path::Path,
) -> std::result::Result<(u64, u64, u64), String> {
    let candidates = index
        .sweep_candidates()
        .map_err(|e| format!("sweep_candidates: {e}"))?;

    let mut segments_removed: u64 = 0;
    let mut bytes_reclaimed: u64 = 0;
    let mut chunks_reclaimed: u64 = 0;

    for seg_id in &candidates {
        let (size_bytes, chunk_count) = index
            .segment_info(*seg_id)
            .map_err(|e| format!("segment_info: {e}"))?;

        let seg_path = segments_dir.join(format!("{:016x}.seg", seg_id));
        match fs::remove_file(&seg_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove_file: {e}")),
        }

        index
            .drop_segment(*seg_id)
            .map_err(|e| format!("drop_segment: {e}"))?;

        segments_removed += 1;
        bytes_reclaimed += size_bytes;
        chunks_reclaimed += chunk_count;
    }

    if segments_removed > 0 {
        let dir_fd = fs::File::open(segments_dir).map_err(|e| format!("open dir: {e}"))?;
        dir_fd.sync_all().map_err(|e| format!("fsync dir: {e}"))?;
    }

    Ok((segments_removed, bytes_reclaimed, chunks_reclaimed))
}

fn run_sweep_idempotence_test(scenario: &SweepScenario) -> std::result::Result<(), TestCaseError> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db_path = root.join("index.db");
    let segments_dir = root.join("segments");
    fs::create_dir_all(&segments_dir).expect("mkdir");

    let index = Index::open(&db_path).expect("open index");

    let mut seg_ids: Vec<u64> = Vec::new();
    let mut file_hashes: Vec<[u8; 32]> = Vec::new();
    let mut chunk_pairs_by_segment: Vec<Vec<([u8; 32], u64)>> = Vec::new();

    // Create and seal each segment.
    for (seg_idx, seg_chunks) in scenario.segments.iter().enumerate() {
        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");
        seg_ids.push(seg_id);

        let mut writer =
            SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");

        // Insert a dummy file row for FK satisfaction.
        let file_hash = test_file_hash(seg_idx);
        index.insert_file(&file_hash, 0).expect("insert file");
        file_hashes.push(file_hash);

        let mut pending_rows = Vec::new();
        let mut chunk_pairs = Vec::new();
        for (chunk_idx, chunk_data) in seg_chunks.iter().enumerate() {
            let loc = writer.append(chunk_data).expect("append");
            let chunk_hash = test_chunk_hash(seg_idx, chunk_idx);
            pending_rows.push(PendingRow {
                chunk_hash,
                file_hash,
                chunk_index: chunk_idx as i64,
                size: i64::from(loc.length),
                segment_id: seg_id,
                segment_offset: loc.offset,
            });
            chunk_pairs.push((chunk_hash, u64::from(loc.length)));
        }
        chunk_pairs_by_segment.push(chunk_pairs);

        index.insert_pending(&pending_rows).expect("insert pending");
        let committed_size = writer.write_offset();
        index.flush_pending(seg_id, committed_size).expect("flush");

        // Seal the segment.
        writer.seal(&segments_dir).expect("seal");

        // Mark as sealed in the index.
        index
            .seal_segment(seg_id, committed_size)
            .expect("seal in index");
    }

    for idx in 0..scenario.segments.len() {
        if scenario.live_segment_indices.contains(&idx) {
            index
                .insert_chunks_for_file(&file_hashes[idx], &chunk_pairs_by_segment[idx])
                .expect("register live file chunks");
        } else {
            index
                .delete_pending_for_segment(seg_ids[idx])
                .expect("retire dead pending rows");
        }
    }

    let dead_count = scenario
        .segments
        .iter()
        .enumerate()
        .filter(|(idx, _)| !scenario.live_segment_indices.contains(idx))
        .count() as u64;

    // First sweep: should remove all dead segments.
    let (removed1, _bytes1, _chunks1) =
        do_sweep(&index, &segments_dir).map_err(TestCaseError::fail)?;

    prop_assert_eq!(
        removed1,
        dead_count,
        "first sweep should remove all dead segments"
    );

    // Verify dead segment files are gone.
    for (idx, _) in scenario.segments.iter().enumerate() {
        if !scenario.live_segment_indices.contains(&idx) {
            let seg_path = segments_dir.join(format!("{:016x}.seg", seg_ids[idx]));
            prop_assert!(!seg_path.exists(), "dead segment file should be removed");
        }
    }

    // Verify live segment files still exist.
    for &live_idx in &scenario.live_segment_indices {
        let seg_path = segments_dir.join(format!("{:016x}.seg", seg_ids[live_idx]));
        prop_assert!(seg_path.exists(), "live segment file should still exist");
    }

    // Second sweep: should be a no-op (idempotent).
    let (removed2, bytes2, chunks2) =
        do_sweep(&index, &segments_dir).map_err(TestCaseError::fail)?;

    prop_assert_eq!(
        removed2,
        0,
        "second sweep should remove 0 segments (idempotent)"
    );
    prop_assert_eq!(
        bytes2,
        0,
        "second sweep should reclaim 0 bytes (idempotent)"
    );
    prop_assert_eq!(
        chunks2,
        0,
        "second sweep should reclaim 0 chunks (idempotent)"
    );

    Ok(())
}
