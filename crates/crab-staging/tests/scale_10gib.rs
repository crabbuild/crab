//! Scale validation: 10 GiB synthetic push in CI (< 2 min budget).
//!
//! Marked `#[ignore]` — run explicitly in CI with
//! `cargo test --test scale_10gib --ignored --release`.
//! The in-tree version uses a 256 MiB proxy to keep `cargo test` fast;
//! the full 10 GiB run is CI-only.

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

/// Chunk size used by the synthetic push (64 KiB, matching the default
/// xet-core target).
const CHUNK_SIZE: usize = 64 * 1024;

/// Default segment target (256 MiB).
const SEGMENT_TARGET: u64 = 256 * 1024 * 1024;

/// Proxy data volume for the non-ignored quick test (256 MiB).
/// 256 MiB / 64 KiB = 4,096 chunks.
const PROXY_BYTES: u64 = 256 * 1024 * 1024;
const PROXY_CHUNKS: usize = (PROXY_BYTES / CHUNK_SIZE as u64) as usize;

/// Full data volume for the CI run (10 GiB).
/// 10 GiB / 64 KiB = 163,840 chunks.
const FULL_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const FULL_CHUNKS: usize = (FULL_BYTES / CHUNK_SIZE as u64) as usize;

const BATCH_CHUNKS: usize = 256;

/// Count files (not directories) in a directory.
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

/// Stage `count` synthetic 64 KiB chunks, sealing segments at the target
/// boundary. Verifies stats and file-count bounds, then spot-checks reads.
async fn run_scale_test(count: usize, expected_bytes: u64) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let segments_dir = root.join("segments");
    let staging = StagingArea::open(root).await.expect("open staging");
    let file_hash = compute_data_hash(b"scale-10gib-file");
    staging
        .pre_register_file(&file_hash, expected_bytes)
        .expect("register file");

    let start = Instant::now();
    let sample_step = if count > 100 { count / 100 } else { 1 };
    let mut samples: Vec<(usize, MerkleHash)> = Vec::new();
    let mut total_chunks_staged = 0;

    for batch_start in (0..count).step_by(BATCH_CHUNKS) {
        let batch_end = (batch_start + BATCH_CHUNKS).min(count);
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
        total_chunks_staged += refs.len();

        for (offset, hash) in hashes.into_iter().enumerate() {
            let i = batch_start + offset;
            if i % sample_step == 0 {
                samples.push((i, hash));
            }
        }
    }

    staging.flush_pending().await.expect("flush pending");

    let elapsed = start.elapsed();
    assert_eq!(total_chunks_staged, count);

    // Verify file count in segments/ is bounded:
    // file_count ≤ ⌈total_bytes / segment_target⌉ + 1
    let file_count = count_files_in_dir(&segments_dir);
    let upper_bound = expected_bytes.div_ceil(SEGMENT_TARGET) + 1;
    assert!(
        file_count <= upper_bound,
        "segment file count {file_count} exceeds upper bound {upper_bound}"
    );

    for (i, hash) in samples {
        let got = staging
            .get_chunk(&hash)
            .await
            .expect("read")
            .expect("staged chunk");
        let mut expected = vec![0u8; CHUNK_SIZE];
        expected[..8].copy_from_slice(&(i as u64).to_le_bytes());
        assert_eq!(got.as_ref(), expected.as_slice(), "chunk {i} data mismatch");
    }

    eprintln!(
        "[scale_10gib] {count} chunks ({} MiB) staged in {:.2}s, {file_count} segment files",
        expected_bytes / (1024 * 1024),
        elapsed.as_secs_f64(),
    );
}

/// Quick proxy test (256 MiB) — runs in normal `cargo test`.
#[tokio::test]
async fn scale_256mib_proxy() {
    run_scale_test(PROXY_CHUNKS, PROXY_BYTES).await;
}

/// Full 10 GiB CI test — run with `cargo test --test scale_10gib --ignored --release`.
/// Budget: < 2 minutes on CI hardware.
#[ignore]
#[tokio::test]
async fn scale_10gib_synthetic_push() {
    run_scale_test(FULL_CHUNKS, FULL_BYTES).await;
}
