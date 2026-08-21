//! Property P3 — Torn-tail recovery: write N chunks, randomly truncate
//! the current segment at a byte ≤ last committed offset, re-open;
//! every chunk whose locator is in `chunks` reads back correctly, and
//! no extras appear.
//!
//! **Validates: Requirements S3.3, I1**

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;

use crate::index::{Index, PendingRow};
use crate::recovery;
use crate::segment::{SegmentReader, SegmentWriter};
use proptest::prelude::*;

/// Fixed file hash for the test.
const TEST_FILE_HASH: [u8; 32] = [0xF2; 32];

/// Deterministic chunk hash from an index.
fn test_chunk_hash(idx: usize) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&(idx as u64).to_le_bytes());
    h
}

/// Generate a vec of chunk data with varying sizes (1–512 bytes) and a
/// commit fraction (0.0–1.0) that determines how many chunks are committed.
fn test_input_strategy() -> impl Strategy<Value = (Vec<Vec<u8>>, f64)> {
    (
        prop::collection::vec(
            prop::collection::vec(any::<u8>(), 1..=512usize),
            1..=20usize,
        ),
        0.0..=1.0f64,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn torn_tail_recovery_preserves_committed_chunks(
        (chunks, commit_frac) in test_input_strategy(),
    ) {
        let total = chunks.len();
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "total is small (≤20), fraction is [0,1]"
        )]
        let commit_count = (total as f64 * commit_frac).round() as usize;
        let commit_count = commit_count.min(total);

        run_torn_tail_test(&chunks, commit_count)?;
    }
}

fn run_torn_tail_test(
    chunks: &[Vec<u8>],
    commit_count: usize,
) -> std::result::Result<(), TestCaseError> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db_path = root.join("index.db");
    let segments_dir = root.join("segments");
    fs::create_dir_all(&segments_dir).expect("mkdir");

    let index = Index::open(&db_path).expect("open index");
    let seg_id = index.allocate_segment_id().expect("alloc");
    index.register_current_segment(seg_id).expect("register");

    // Insert a dummy file row so FK constraints are satisfied.
    index.insert_file(&TEST_FILE_HASH, 0).expect("insert file");

    let mut writer = SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");

    // Append all chunks and track their locators.
    let mut locators = Vec::new();
    for chunk_data in chunks {
        let loc = writer.append(chunk_data).expect("append");
        locators.push(loc);
    }

    // Commit the first `commit_count` chunks by flushing them.
    if commit_count > 0 {
        let pending_rows: Vec<PendingRow> = locators[..commit_count]
            .iter()
            .enumerate()
            .map(|(idx, loc)| PendingRow {
                chunk_hash: test_chunk_hash(idx),
                file_hash: TEST_FILE_HASH,
                chunk_index: idx as i64,
                size: i64::from(loc.length),
                segment_id: seg_id,
                segment_offset: loc.offset,
            })
            .collect();

        index.insert_pending(&pending_rows).expect("insert pending");

        let last_committed = &locators[commit_count - 1];
        let committed_size = last_committed.offset + u64::from(last_committed.length) + 8;
        index.flush_pending(seg_id, committed_size).expect("flush");
    }

    // If there are uncommitted chunks, insert them as pending (simulating
    // writes that happened after the last flush but before the crash).
    if commit_count < chunks.len() {
        let uncommitted_pending: Vec<PendingRow> = locators[commit_count..]
            .iter()
            .enumerate()
            .map(|(idx, loc)| PendingRow {
                chunk_hash: test_chunk_hash(commit_count + idx),
                file_hash: TEST_FILE_HASH,
                chunk_index: (commit_count + idx) as i64,
                size: i64::from(loc.length),
                segment_id: seg_id,
                segment_offset: loc.offset,
            })
            .collect();

        index
            .insert_pending(&uncommitted_pending)
            .expect("insert uncommitted pending");
    }

    // Drop the writer so the file handle is released.
    drop(writer);

    // Run recovery — this truncates current.seg to max_committed_offset
    // and deletes pending rows.
    let recovered = recovery::recover(&root, &index).expect("recover");
    let rec = recovered.expect("should have a current segment");
    prop_assert_eq!(rec.segment_id, seg_id);

    // Verify the file was truncated correctly.
    let expected_offset = if commit_count > 0 {
        let last = &locators[commit_count - 1];
        last.offset + u64::from(last.length) + 8
    } else {
        0
    };
    prop_assert_eq!(rec.write_offset, expected_offset);

    let file_size = fs::metadata(segments_dir.join("current.seg"))
        .expect("stat")
        .len();
    prop_assert_eq!(file_size, expected_offset);

    // Verify all committed chunks read back correctly.
    if commit_count > 0 {
        let reader =
            SegmentReader::open(&segments_dir.join("current.seg"), seg_id).expect("reader");

        for (idx, loc) in locators[..commit_count].iter().enumerate() {
            let data = reader
                .read(loc.offset, loc.length)
                .expect("read committed chunk");
            prop_assert_eq!(
                data.as_ref(),
                chunks[idx].as_slice(),
                "committed chunk {} data mismatch",
                idx
            );
        }
    }

    // Verify no pending rows remain (uncommitted chunks are gone).
    for idx in commit_count..chunks.len() {
        let hash = test_chunk_hash(idx);
        let pending = index.locate_pending(&hash).expect("locate pending");
        prop_assert!(
            pending.is_none(),
            "pending row for uncommitted chunk {} should be deleted",
            idx
        );
    }

    // Verify committed chunks are still in the index.
    for idx in 0..commit_count {
        let hash = test_chunk_hash(idx);
        let loc = index.locate(&hash).expect("locate committed");
        prop_assert!(
            loc.is_some(),
            "committed chunk {} should still be in index",
            idx
        );
    }

    Ok(())
}
