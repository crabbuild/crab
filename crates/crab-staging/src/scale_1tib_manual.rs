//! Pre-release manual gate: 1 TiB push on a reference host.
//!
//! This test is a skeleton for the manual pre-release validation.
//! The actual 1 TiB run is performed manually on a reference host
//! (NVMe SSD, default ext4, ≥ 2 TiB free). The test documents what
//! to measure and provides the harness for recording results.
//!
//! Run with: `cargo test --test scale_1tib_manual --ignored --release`
//!
//! Measurements to record:
//! - **fsync count**: count of sealed segments (one fsync per seal)
//! - **Wall time**: `Instant::now()` around the staging loop
//! - **Peak inode use**: file count in `segments/` after staging

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::time::Instant;

use crate::index::{Index, PendingRow};
use crate::segment::SegmentWriter;

/// Chunk size (64 KiB).
const CHUNK_SIZE: usize = 64 * 1024;

/// Default segment target (256 MiB).
const SEGMENT_TARGET: u64 = 256 * 1024 * 1024;

/// Default segment hard cap (512 MiB).
const SEGMENT_HARD_CAP: u64 = 512 * 1024 * 1024;

/// 1 TiB in bytes.
const ONE_TIB: u64 = 1024 * 1024 * 1024 * 1024;

/// 1 TiB / 64 KiB = 16,777,216 chunks.
const FULL_CHUNKS: usize = (ONE_TIB / CHUNK_SIZE as u64) as usize;

/// Fixed file hash for the scale test.
const SCALE_FILE_HASH: [u8; 32] = [0xF1; 32];

/// Deterministic chunk hash from an index.
fn test_chunk_hash(idx: usize) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&(idx as u64).to_le_bytes());
    h
}

/// Count files in a directory (non-recursive).
fn count_files_in_dir(dir: &std::path::Path) -> u64 {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_file()) {
                count += 1;
            }
        }
    }
    count
}

/// Manual gate: 1 TiB synthetic push.
///
/// Run on a reference host with sufficient disk. Records:
/// - fsync count (= segments sealed, one fsync per seal)
/// - wall time
/// - peak inode use (file count in segments/)
///
/// Expected bounds:
/// - File count ≤ ⌈1 TiB / 256 MiB⌉ + 1 = 4097
/// - fsync count ≈ segments sealed ≈ 4096
/// - Wall time: target < 10 minutes on NVMe
#[ignore]
#[test]
fn scale_1tib_manual_gate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db_path = root.join("index.db");
    let segments_dir = root.join("segments");
    fs::create_dir_all(&segments_dir).expect("mkdir segments");

    let index = Index::open(&db_path).expect("open index");

    // Insert a dummy file row so FK constraints on chunks are satisfied.
    index.insert_file(&SCALE_FILE_HASH, 0).expect("insert file");

    let start = Instant::now();
    let mut segments_sealed: u64 = 0;

    let mut seg_id = index.allocate_segment_id().expect("alloc");
    index.register_current_segment(seg_id).expect("register");
    let mut writer = SegmentWriter::new(&segments_dir, seg_id, SEGMENT_TARGET, SEGMENT_HARD_CAP)
        .expect("writer");

    for i in 0..FULL_CHUNKS {
        let mut data = vec![0u8; CHUNK_SIZE];
        data[..8].copy_from_slice(&(i as u64).to_le_bytes());
        let chunk_hash = test_chunk_hash(i);

        let loc = writer.append(&data).expect("append");

        index
            .insert_pending(&[PendingRow {
                chunk_hash,
                file_hash: SCALE_FILE_HASH,
                chunk_index: i as i64,
                size: i64::from(loc.length),
                segment_id: seg_id,
                segment_offset: loc.offset,
            }])
            .expect("insert pending");

        if writer.should_seal() {
            let committed_size = writer.write_offset();
            index.flush_pending(seg_id, committed_size).expect("flush");
            writer.seal(&segments_dir).expect("seal");
            index
                .seal_segment(seg_id, committed_size)
                .expect("seal in index");
            segments_sealed += 1;

            seg_id = index.allocate_segment_id().expect("alloc next");
            index
                .register_current_segment(seg_id)
                .expect("register next");
            writer = SegmentWriter::new(&segments_dir, seg_id, SEGMENT_TARGET, SEGMENT_HARD_CAP)
                .expect("writer next");
        }

        // Progress reporting every 1M chunks (~64 GiB).
        if i > 0 && i % 1_000_000 == 0 {
            let elapsed = start.elapsed();
            let gib_done = (i as f64 * CHUNK_SIZE as f64) / (1024.0 * 1024.0 * 1024.0);
            eprintln!(
                "[manual gate] {gib_done:.1} GiB staged in {:.1}s ({segments_sealed} segments sealed)",
                elapsed.as_secs_f64()
            );
        }
    }

    // Flush remaining.
    let final_size = writer.write_offset();
    if final_size > 0 {
        index
            .flush_pending(seg_id, final_size)
            .expect("flush final");
    }

    let wall_time = start.elapsed();
    let peak_inode_use = count_files_in_dir(&segments_dir);
    let expected_max_files = ONE_TIB.div_ceil(SEGMENT_TARGET) + 1;

    // Print results for manual recording.
    eprintln!("=== 1 TiB Manual Gate Results ===");
    eprintln!("Wall time:       {:.1}s", wall_time.as_secs_f64());
    eprintln!("Segments sealed: {segments_sealed}");
    eprintln!("Peak inode use:  {peak_inode_use} files in segments/");
    eprintln!("File count bound: {expected_max_files}");
    eprintln!("=================================");

    assert!(
        peak_inode_use <= expected_max_files,
        "peak inode use {peak_inode_use} exceeds bound {expected_max_files}"
    );
}
