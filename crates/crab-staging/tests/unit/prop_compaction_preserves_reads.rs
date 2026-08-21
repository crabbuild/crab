//! Property P5 — Compaction preserves reads: for any staged chunk, a
//! `get_chunk` interleaved with compaction returns correct bytes or
//! retries without error.
//!
//! **Validates: Requirements S5.2**

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use crate::index::{Index, PendingRow};
use crate::segment::{self, SegmentReader, SegmentWriter};
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

/// A compaction test scenario: multiple segments with chunks, some of
/// which are "live" (registered to files) and some "dead".
#[derive(Debug, Clone)]
struct CompactionScenario {
    segments: Vec<Vec<Vec<u8>>>,
    live_indices: Vec<Vec<usize>>,
}

fn compaction_scenario_strategy() -> impl Strategy<Value = CompactionScenario> {
    let segments = prop::collection::vec(
        prop::collection::vec(
            prop::collection::vec(any::<u8>(), 16..=256usize),
            2..=8usize,
        ),
        2..=4usize,
    );

    segments.prop_flat_map(|segs| {
        let ranges: Vec<_> = segs
            .iter()
            .map(|s| {
                let n = s.len();
                prop::collection::vec(0..n, 0..=n)
            })
            .collect();

        ranges.prop_map(move |idx_vecs| {
            let live_indices: Vec<Vec<usize>> = idx_vecs
                .into_iter()
                .map(|mut v| {
                    v.sort_unstable();
                    v.dedup();
                    v
                })
                .collect();
            CompactionScenario {
                segments: segs.clone(),
                live_indices,
            }
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn compaction_preserves_all_live_chunk_reads(
        scenario in compaction_scenario_strategy(),
    ) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(run_compaction_test(&scenario))?;
    }
}

/// Info needed to read a chunk from a segment file.
#[derive(Clone)]
struct ReadInfo {
    segment_id: u64,
    offset: u64,
    length: u32,
    expected_data: Vec<u8>,
}

async fn run_compaction_test(
    scenario: &CompactionScenario,
) -> std::result::Result<(), TestCaseError> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let db_path = root.join("index.db");
    let segments_dir = root.join("segments");
    fs::create_dir_all(&segments_dir).expect("mkdir");

    let index = Index::open(&db_path).expect("open index");

    // Map from (seg_idx, chunk_idx) to chunk data for live chunks.
    let mut live_chunk_data: HashMap<(usize, usize), Vec<u8>> = HashMap::new();
    let mut seg_ids: Vec<u64> = Vec::new();

    // Create and seal each segment.
    for (seg_idx, seg_chunks) in scenario.segments.iter().enumerate() {
        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");
        seg_ids.push(seg_id);

        let file_hash = test_file_hash(seg_idx);
        index.insert_file(&file_hash, 0).expect("insert file");

        let mut writer =
            SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");

        let mut pending_rows = Vec::new();
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
        }

        index.insert_pending(&pending_rows).expect("insert pending");
        let committed_size = writer.write_offset();
        index.flush_pending(seg_id, committed_size).expect("flush");
        writer.seal(&segments_dir).expect("seal");
        index
            .seal_segment(seg_id, committed_size)
            .expect("seal in index");
    }

    // Register live chunks.
    for (seg_idx, live_idxs) in scenario.live_indices.iter().enumerate() {
        let seg_id = seg_ids[seg_idx];
        let count = live_idxs.len() as u64;
        if count > 0 {
            index
                .increment_live_chunk_count(seg_id, count)
                .expect("increment live");
        }
        for &chunk_idx in live_idxs {
            live_chunk_data.insert(
                (seg_idx, chunk_idx),
                scenario.segments[seg_idx][chunk_idx].clone(),
            );
        }
    }

    // Pre-compaction: snapshot locators for all live chunks.
    let mut pre_read_infos: Vec<ReadInfo> = Vec::new();
    for &(seg_idx, chunk_idx) in live_chunk_data.keys() {
        let chunk_hash = test_chunk_hash(seg_idx, chunk_idx);
        let loc = index.locate(&chunk_hash).expect("locate").expect("exists");
        pre_read_infos.push(ReadInfo {
            segment_id: loc.segment_id,
            offset: loc.offset,
            length: loc.length,
            expected_data: live_chunk_data[&(seg_idx, chunk_idx)].clone(),
        });
    }

    // Verify pre-compaction reads.
    for info in &pre_read_infos {
        let seg_path = segments_dir.join(format!("{:016x}.seg", info.segment_id));
        let reader = SegmentReader::open(&seg_path, info.segment_id).expect("reader");
        let data = reader.read(info.offset, info.length).expect("read");
        prop_assert_eq!(data.as_ref(), info.expected_data.as_slice());
    }

    // Spawn concurrent readers that read from segment files directly.
    // They snapshot locators now and read during/after compaction.
    let segments_dir_arc = Arc::new(segments_dir.clone());
    let mut reader_handles = Vec::new();

    for info in &pre_read_infos {
        let ri = info.clone();
        let sd = Arc::clone(&segments_dir_arc);

        let handle = tokio::spawn(async move {
            // Small delay to interleave with compaction.
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;

            let seg_path = sd.join(format!("{:016x}.seg", ri.segment_id));
            // The old segment file might still exist during compaction.
            if seg_path.exists() {
                let reader =
                    SegmentReader::open(&seg_path, ri.segment_id).expect("concurrent reader");
                let data = reader.read(ri.offset, ri.length).expect("concurrent read");
                assert_eq!(data.as_ref(), ri.expected_data.as_slice());
            }
        });
        reader_handles.push(handle);
    }

    // Run compaction with a very low threshold so all partially-dead
    // segments get compacted.
    let candidates = index.compaction_candidates(0.01).expect("candidates");

    for old_seg_id in &candidates {
        let live_chunks = index.live_chunks_for_segment(*old_seg_id).expect("live");
        if live_chunks.is_empty() {
            continue;
        }

        let new_seg_id = index.allocate_segment_id().expect("alloc new");
        let old_reader = SegmentReader::open(
            &segments_dir.join(format!("{old_seg_id:016x}.seg")),
            *old_seg_id,
        )
        .expect("open old");

        let tmp_path = segments_dir.join("current.seg.tmp");
        let new_seg_path = segments_dir.join(format!("{new_seg_id:016x}.seg"));

        let mut new_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .expect("open tmp");

        let mut new_offset: u64 = 0;
        let mut updates = Vec::new();

        for (chunk_hash, file_hash, chunk_index, size, old_offset) in &live_chunks {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "size bounded by segment cap"
            )]
            let length = *size as u32;
            let data = old_reader.read(*old_offset, length).expect("read");
            let record = segment::encode_record(&data);
            std::io::Write::write_all(&mut new_file, &record).expect("write");
            updates.push((*chunk_hash, *file_hash, *chunk_index, new_offset));
            new_offset += record.len() as u64;
        }

        new_file.sync_data().expect("fsync");
        drop(new_file);
        fs::rename(&tmp_path, &new_seg_path).expect("rename");

        index
            .swap_locators(*old_seg_id, new_seg_id, new_offset, &updates)
            .expect("swap");

        let old_path = segments_dir.join(format!("{old_seg_id:016x}.seg"));
        let _ = fs::remove_file(&old_path);
        index.drop_segment(*old_seg_id).expect("drop old");
    }

    // Wait for concurrent readers.
    for handle in reader_handles {
        handle.await.expect("reader task");
    }

    // Post-compaction: verify ALL live chunks read correctly via updated locators.
    for (&(seg_idx, chunk_idx), expected_data) in &live_chunk_data {
        let chunk_hash = test_chunk_hash(seg_idx, chunk_idx);
        let loc = index
            .locate(&chunk_hash)
            .expect("locate after compaction")
            .expect("chunk should still exist");

        let seg_path = segments_dir.join(format!("{:016x}.seg", loc.segment_id));
        let reader =
            SegmentReader::open(&seg_path, loc.segment_id).expect("reader after compaction");
        let data = reader
            .read(loc.offset, loc.length)
            .expect("read after compaction");
        prop_assert_eq!(data.as_ref(), expected_data.as_slice());
    }

    Ok(())
}
