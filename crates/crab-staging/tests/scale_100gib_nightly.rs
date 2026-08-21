//! File-count bound: after staging B bytes of distinct
//! chunks, file count in `segments/` ≤ ⌈B / segment_target_bytes⌉ + 1.
//! Marked `#[ignore]` — the full 100 GiB run is nightly-only.
//! The in-tree version uses a 512 MiB proxy; run the nightly variant
//! with `cargo test --test scale_100gib_nightly --ignored`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;

use crab_staging::StagingArea;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use proptest::prelude::*;

/// Default segment target (256 MiB), matching `StagingConfig::default()`.
const SEGMENT_TARGET_BYTES: u64 = 256 * 1024 * 1024;

/// Chunk size for synthetic data (64 KiB).
const CHUNK_SIZE: usize = 64 * 1024;

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

/// Stage `total_bytes` worth of distinct 64 KiB chunks through the public API.
fn stage_bytes_and_seal(root: &std::path::Path, total_bytes: u64) -> std::path::PathBuf {
    let segments_dir = root.join("segments");
    let chunk_count = (total_bytes / CHUNK_SIZE as u64) as usize;
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        let staging = StagingArea::open(root.to_path_buf())
            .await
            .expect("open staging");
        let file_hash = compute_data_hash(b"scale-100gib-file");
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("register file");

        for batch_start in (0..chunk_count).step_by(BATCH_CHUNKS) {
            let batch_end = (batch_start + BATCH_CHUNKS).min(chunk_count);
            let data: Vec<Vec<u8>> = (batch_start..batch_end)
                .map(|i| {
                    let mut chunk = vec![0u8; CHUNK_SIZE];
                    chunk[..8].copy_from_slice(&(i as u64).to_le_bytes());
                    chunk
                })
                .collect();
            let hashes: Vec<MerkleHash> =
                data.iter().map(|chunk| compute_data_hash(chunk)).collect();
            let refs: Vec<(&MerkleHash, &[u8])> = hashes
                .iter()
                .zip(&data)
                .map(|(hash, chunk)| (hash, chunk.as_slice()))
                .collect();
            staging
                .stage_chunks_batch(&refs, &file_hash, batch_start as u64)
                .await
                .expect("stage batch");
        }
        staging.flush_pending().await.expect("flush pending");
    });

    segments_dir
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    /// Proxy: for a random data volume between 64 MiB and
    /// 512 MiB, the file count in `segments/` never exceeds
    /// ⌈total_bytes / segment_target_bytes⌉ + 1.
    #[test]
    fn file_count_bounded_by_staged_bytes(
        // Random volume between 64 MiB and 512 MiB (in 64 KiB increments).
        num_chunks in 1024u64..=8192u64,
    ) {
        let total_bytes = num_chunks * CHUNK_SIZE as u64;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let segments_dir = stage_bytes_and_seal(&root, total_bytes);

        let file_count = count_files_in_dir(&segments_dir);
        let upper_bound = total_bytes.div_ceil(SEGMENT_TARGET_BYTES) + 1;

        prop_assert!(
            file_count <= upper_bound,
            "file_count={file_count} exceeds ⌈{total_bytes}/{SEGMENT_TARGET_BYTES}⌉+1={upper_bound}"
        );
    }
}

/// Full 100 GiB nightly test — run with
/// `cargo test --test scale_100gib_nightly --ignored --release`.
///
/// Asserts: file count in `segments` is bounded by 256 MiB segments.
#[ignore]
#[test]
fn scale_100gib_file_count_bound() {
    let total_bytes: u64 = 100 * 1024 * 1024 * 1024;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();

    let segments_dir = stage_bytes_and_seal(&root, total_bytes);

    let file_count = count_files_in_dir(&segments_dir);
    let upper_bound = total_bytes.div_ceil(SEGMENT_TARGET_BYTES) + 1;

    assert!(
        file_count <= upper_bound,
        "file_count={file_count} exceeds ⌈{total_bytes}/{SEGMENT_TARGET_BYTES}⌉+1={upper_bound}"
    );
}
