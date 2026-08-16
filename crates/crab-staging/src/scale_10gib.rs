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

use crate::index::{Index, PendingRow};
use crate::segment::{SegmentReader, SegmentWriter};

/// Chunk size used by the synthetic push (64 KiB, matching the default
/// xet-core target).
const CHUNK_SIZE: usize = 64 * 1024;

/// Default segment target (256 MiB).
const SEGMENT_TARGET: u64 = 256 * 1024 * 1024;

/// Default segment hard cap (512 MiB).
const SEGMENT_HARD_CAP: u64 = 512 * 1024 * 1024;

/// Proxy data volume for the non-ignored quick test (256 MiB).
/// 256 MiB / 64 KiB = 4,096 chunks.
const PROXY_BYTES: u64 = 256 * 1024 * 1024;
const PROXY_CHUNKS: usize = (PROXY_BYTES / CHUNK_SIZE as u64) as usize;

/// Full data volume for the CI run (10 GiB).
/// 10 GiB / 64 KiB = 163,840 chunks.
const FULL_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const FULL_CHUNKS: usize = (FULL_BYTES / CHUNK_SIZE as u64) as usize;

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

/// Stage `count` synthetic 64 KiB chunks, sealing segments at the target
/// boundary. Verifies stats and file-count bounds, then spot-checks reads.
fn run_scale_test(count: usize, expected_bytes: u64) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db_path = root.join("index.db");
    let segments_dir = root.join("segments");
    fs::create_dir_all(&segments_dir).expect("mkdir segments");

    let index = Index::open(&db_path).expect("open index");

    // Insert a dummy file row so FK constraints on chunks are satisfied.
    index.insert_file(&SCALE_FILE_HASH, 0).expect("insert file");

    let start = Instant::now();

    let mut seg_id = index.allocate_segment_id().expect("alloc");
    index.register_current_segment(seg_id).expect("register");
    let mut writer = SegmentWriter::new(&segments_dir, seg_id, SEGMENT_TARGET, SEGMENT_HARD_CAP)
        .expect("writer");

    // Track a sample of locators for read-back verification.
    let sample_step = if count > 100 { count / 100 } else { 1 };
    let mut sample_locators: Vec<(usize, u64, u64, u32)> = Vec::new(); // (idx, seg_id, offset, len)

    let mut total_chunks_staged: u64 = 0;

    for i in 0..count {
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

        total_chunks_staged += 1;

        if i % sample_step == 0 {
            sample_locators.push((i, seg_id, loc.offset, loc.length));
        }

        // Seal when we hit the soft cap.
        if writer.should_seal() {
            let committed_size = writer.write_offset();
            index.flush_pending(seg_id, committed_size).expect("flush");
            writer.seal(&segments_dir).expect("seal");
            index
                .seal_segment(seg_id, committed_size)
                .expect("seal in index");

            seg_id = index.allocate_segment_id().expect("alloc next");
            index
                .register_current_segment(seg_id)
                .expect("register next");
            writer = SegmentWriter::new(&segments_dir, seg_id, SEGMENT_TARGET, SEGMENT_HARD_CAP)
                .expect("writer next");
        }
    }

    // Flush remaining pending chunks.
    let final_size = writer.write_offset();
    if final_size > 0 {
        index
            .flush_pending(seg_id, final_size)
            .expect("flush final");
    }

    let elapsed = start.elapsed();

    // Verify total chunk count matches.
    assert_eq!(total_chunks_staged, count as u64);

    // Verify file count in segments/ is bounded:
    // file_count ≤ ⌈total_bytes / segment_target⌉ + 1
    let file_count = count_files_in_dir(&segments_dir);
    let upper_bound = expected_bytes.div_ceil(SEGMENT_TARGET) + 1;
    assert!(
        file_count <= upper_bound,
        "segment file count {file_count} exceeds upper bound {upper_bound}"
    );

    // Spot-check reads on sampled chunks.
    for (i, sample_seg_id, offset, length) in &sample_locators {
        let seg_path = if *sample_seg_id == seg_id {
            // Current (unsealed) segment.
            segments_dir.join("current.seg")
        } else {
            segments_dir.join(format!("{sample_seg_id:016x}.seg"))
        };
        let reader = SegmentReader::open(&seg_path, *sample_seg_id).expect("reader");
        let got = reader.read(*offset, *length).expect("read");

        let mut expected = vec![0u8; CHUNK_SIZE];
        expected[..8].copy_from_slice(&(*i as u64).to_le_bytes());
        assert_eq!(got.as_ref(), expected.as_slice(), "chunk {i} data mismatch");
    }

    eprintln!(
        "[scale_10gib] {count} chunks ({} MiB) staged in {:.2}s, {file_count} segment files",
        expected_bytes / (1024 * 1024),
        elapsed.as_secs_f64(),
    );
}

/// Quick proxy test (256 MiB) — runs in normal `cargo test`.
#[test]
fn scale_256mib_proxy() {
    run_scale_test(PROXY_CHUNKS, PROXY_BYTES);
}

/// Full 10 GiB CI test — run with `cargo test --test scale_10gib --ignored --release`.
/// Budget: < 2 minutes on CI hardware.
#[ignore]
#[test]
fn scale_10gib_synthetic_push() {
    run_scale_test(FULL_CHUNKS, FULL_BYTES);
}
