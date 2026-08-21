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

use crab_staging::StagingArea;
use crab_xet::hash::{MerkleHash, compute_data_hash};

/// Chunk size (64 KiB).
const CHUNK_SIZE: usize = 64 * 1024;

/// Default segment target (256 MiB).
const SEGMENT_TARGET: u64 = 256 * 1024 * 1024;

/// 1 TiB in bytes.
const ONE_TIB: u64 = 1024 * 1024 * 1024 * 1024;

/// 1 TiB / 64 KiB = 16,777,216 chunks.
const FULL_CHUNKS: usize = (ONE_TIB / CHUNK_SIZE as u64) as usize;

const BATCH_CHUNKS: usize = 256;

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
#[tokio::test]
async fn scale_1tib_manual_gate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let segments_dir = root.join("segments");
    let staging = StagingArea::open(root).await.expect("open staging");
    let file_hash = compute_data_hash(b"scale-1tib-file");
    staging
        .pre_register_file(&file_hash, ONE_TIB)
        .expect("register file");

    let start = Instant::now();
    for batch_start in (0..FULL_CHUNKS).step_by(BATCH_CHUNKS) {
        let batch_end = (batch_start + BATCH_CHUNKS).min(FULL_CHUNKS);
        let data: Vec<Vec<u8>> = (batch_start..batch_end)
            .map(|i| {
                let mut chunk = vec![0u8; CHUNK_SIZE];
                chunk[..8].copy_from_slice(&(i as u64).to_le_bytes());
                chunk
            })
            .collect();
        let hashes: Vec<MerkleHash> = data.iter().map(|chunk| compute_data_hash(chunk)).collect();
        let refs: Vec<(&MerkleHash, &[u8])> = hashes
            .iter()
            .zip(&data)
            .map(|(hash, chunk)| (hash, chunk.as_slice()))
            .collect();
        staging
            .stage_chunks_batch(&refs, &file_hash, batch_start as u64)
            .await
            .expect("stage batch");

        // Progress reporting approximately every 1M chunks (~64 GiB).
        if batch_start > 0 && batch_start % 1_000_000 < BATCH_CHUNKS {
            let elapsed = start.elapsed();
            let gib_done = (batch_start as f64 * CHUNK_SIZE as f64) / (1024.0 * 1024.0 * 1024.0);
            let segments_sealed = count_files_in_dir(&segments_dir).saturating_sub(1);
            eprintln!(
                "[manual gate] {gib_done:.1} GiB staged in {:.1}s ({segments_sealed} segments sealed)",
                elapsed.as_secs_f64()
            );
        }
    }

    staging.flush_pending().await.expect("flush pending");

    let wall_time = start.elapsed();
    let peak_inode_use = count_files_in_dir(&segments_dir);
    let segments_sealed = peak_inode_use.saturating_sub(1);
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
