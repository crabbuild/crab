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

use crate::index::{Index, PendingRow};
use crate::segment::SegmentWriter;
use proptest::prelude::*;

/// Default segment target (256 MiB), matching `StagingConfig::default()`.
const SEGMENT_TARGET_BYTES: u64 = 256 * 1024 * 1024;

/// Default segment hard cap (512 MiB).
const SEGMENT_HARD_CAP: u64 = 512 * 1024 * 1024;

/// Chunk size for synthetic data (64 KiB).
const CHUNK_SIZE: usize = 64 * 1024;

/// Fixed file hash for the scale test.
const SCALE_FILE_HASH: [u8; 32] = [0xF1; 32];

/// Deterministic chunk hash from an index.
fn test_chunk_hash(idx: usize) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&(idx as u64).to_le_bytes());
    h
}

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

/// Stage `total_bytes` worth of 64 KiB chunks using the low-level
/// segment writer + index, sealing segments at the target boundary.
/// Returns the actual number of bytes written and the segments dir path.
fn stage_bytes_and_seal(root: &std::path::Path, total_bytes: u64) -> (u64, std::path::PathBuf) {
    let db_path = root.join("index.db");
    let segments_dir = root.join("segments");
    fs::create_dir_all(&segments_dir).expect("mkdir segments");

    let index = Index::open(&db_path).expect("open index");

    // Insert a dummy file row so FK constraints on chunks are satisfied.
    index.insert_file(&SCALE_FILE_HASH, 0).expect("insert file");

    let chunk_count = (total_bytes / CHUNK_SIZE as u64) as usize;
    let mut bytes_written: u64 = 0;

    let mut seg_id = index.allocate_segment_id().expect("alloc");
    index.register_current_segment(seg_id).expect("register");
    let mut writer = SegmentWriter::new(
        &segments_dir,
        seg_id,
        SEGMENT_TARGET_BYTES,
        SEGMENT_HARD_CAP,
    )
    .expect("writer");

    for i in 0..chunk_count {
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

        bytes_written += u64::from(loc.length) + 8; // data + framing

        // Seal when we hit the soft cap.
        if writer.should_seal() {
            let committed_size = writer.write_offset();
            index.flush_pending(seg_id, committed_size).expect("flush");
            writer.seal(&segments_dir).expect("seal");
            index
                .seal_segment(seg_id, committed_size)
                .expect("seal in index");

            // Open a new segment.
            seg_id = index.allocate_segment_id().expect("alloc next");
            index
                .register_current_segment(seg_id)
                .expect("register next");
            writer = SegmentWriter::new(
                &segments_dir,
                seg_id,
                SEGMENT_TARGET_BYTES,
                SEGMENT_HARD_CAP,
            )
            .expect("writer next");
        }
    }

    // Flush any remaining pending chunks in the current segment.
    let final_size = writer.write_offset();
    if final_size > 0 {
        index
            .flush_pending(seg_id, final_size)
            .expect("flush final");
    }

    (bytes_written, segments_dir)
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

        let (_bytes_written, segments_dir) = stage_bytes_and_seal(&root, total_bytes);

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

    let (_bytes_written, segments_dir) = stage_bytes_and_seal(&root, total_bytes);

    let file_count = count_files_in_dir(&segments_dir);
    let upper_bound = total_bytes.div_ceil(SEGMENT_TARGET_BYTES) + 1;

    assert!(
        file_count <= upper_bound,
        "file_count={file_count} exceeds ⌈{total_bytes}/{SEGMENT_TARGET_BYTES}⌉+1={upper_bound}"
    );
}
