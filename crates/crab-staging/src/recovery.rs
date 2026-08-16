//! Crash recovery for the segment-based staging area.
//!
//! Called from `StagingArea::open`. Handles sealed-segment integrity
//! verification, torn-tail truncation on the current segment, and
//! orphan temp cleanup.

use std::fs;
use std::path::Path;

use tracing::debug;

use crate::error::{Result, StagingError};

use super::index::Index;

/// Information about a recovered current segment, if one existed.
#[derive(Debug)]
pub struct RecoveredSegment {
    /// The segment id of the recovered current segment.
    pub segment_id: u64,
    /// The write offset after truncation (byte position for next append).
    pub write_offset: u64,
}

/// Run crash recovery on the staging area.
///
/// For each segment in the index:
/// - Sealed segments: verify the on-disk file size is at least the
///   recorded `size_bytes`, returning `StagingCorrupt` on mismatch.
/// - Unsealed (current) segment: recover to the shorter of the recorded
///   committed boundary and actual file length, fsync when truncated, and
///   delete pending rows that no longer fit.
///
/// Finally, removes any orphan `current.seg.tmp` files.
///
/// Returns `Some(RecoveredSegment)` if an existing current segment was
/// found and recovered, or `None` if no current segment existed (fresh
/// staging or all segments were sealed).
///
/// # Errors
///
/// Returns [`StagingError::StagingCorrupt`] if a sealed segment is
/// missing or undersized, or [`StagingError::Io`] on filesystem failure.
pub fn recover(root: &Path, index: &Index) -> Result<Option<RecoveredSegment>> {
    let segments_dir = root.join("segments");
    let segments = index.all_segments()?;

    let mut recovered = None;

    for (segment_id, sealed_at, size_bytes) in &segments {
        if sealed_at.is_some() {
            verify_sealed_segment(&segments_dir, *segment_id, *size_bytes)?;
        } else {
            let write_offset = truncate_current_segment(&segments_dir, *segment_id, index)?;
            recovered = Some(RecoveredSegment {
                segment_id: *segment_id,
                write_offset,
            });
        }
    }

    cleanup_orphan_temps(&segments_dir)?;

    debug!("crash recovery complete");
    Ok(recovered)
}

/// Verify a sealed segment file exists and has at least the recorded size.
fn verify_sealed_segment(
    segments_dir: &Path,
    segment_id: u64,
    expected_min_size: u64,
) -> Result<()> {
    let path = segments_dir.join(format!("{segment_id:016x}.seg"));
    let meta = fs::metadata(&path).map_err(|e| {
        StagingError::StagingCorrupt(format!(
            "sealed segment {segment_id:016x}.seg missing or inaccessible: {e}"
        ))
    })?;

    if meta.len() < expected_min_size {
        return Err(StagingError::StagingCorrupt(format!(
            "sealed segment {segment_id:016x}.seg is {} bytes, expected at least {expected_min_size}",
            meta.len()
        )));
    }

    debug!(
        segment_id,
        file_size = meta.len(),
        expected_min = expected_min_size,
        "sealed segment verified"
    );
    Ok(())
}

/// Truncate the current segment to the recovered byte boundary and fsync.
///
/// Returns the write offset after truncation (the byte position for the
/// next append).
fn truncate_current_segment(segments_dir: &Path, segment_id: u64, index: &Index) -> Result<u64> {
    let path = segments_dir.join("current.seg");

    let recorded_offset = index.max_committed_offset(segment_id)?;
    let promoted_offset = index.max_promoted_chunk_offset(segment_id)?;

    // If the file doesn't exist and there are no committed chunks,
    // discard any pending rows and recover an empty current segment.
    if !path.exists() {
        if promoted_offset > 0 {
            return Err(StagingError::StagingCorrupt(format!(
                "current segment {segment_id:016x}.seg missing but promoted chunks require {promoted_offset} bytes"
            )));
        }
        index.delete_pending_beyond_offset(segment_id, 0)?;
        index.flush_pending(segment_id, 0)?;
        debug!(segment_id, "no current.seg to recover");
        return Ok(0);
    }

    let file_size = fs::metadata(&path)?.len();
    if file_size < promoted_offset {
        return Err(StagingError::StagingCorrupt(format!(
            "current segment {segment_id:016x}.seg is {file_size} bytes, but promoted chunks require {promoted_offset}"
        )));
    }

    let recorded_boundary = recorded_offset.min(file_size);
    let pending_offset = index.max_recoverable_pending_offset(segment_id, recorded_boundary)?;
    let recovered_offset = promoted_offset.max(pending_offset);
    let file = fs::OpenOptions::new().write(true).open(&path)?;

    if file_size > recovered_offset {
        file.set_len(recovered_offset)?;
        file.sync_data()?;
    }

    // Discard pending rows whose full framed record crosses the recovered
    // boundary. Rows that start before the boundary but end after it are
    // incomplete and must not survive recovery.
    index.delete_pending_beyond_offset(segment_id, recovered_offset)?;
    // Keep SQLite's durable boundary in lockstep with the recovered file
    // length so future recovery and abandoned-segment cleanup do not reason
    // from a stale pre-crash size.
    index.flush_pending(segment_id, recovered_offset)?;

    debug!(
        segment_id,
        recorded_offset, file_size, pending_offset, recovered_offset, "current segment recovered"
    );
    Ok(recovered_offset)
}

/// Remove orphan `current.seg.tmp` files from the segments directory.
fn cleanup_orphan_temps(segments_dir: &Path) -> Result<()> {
    let tmp_path = segments_dir.join("current.seg.tmp");
    if tmp_path.exists() {
        fs::remove_file(&tmp_path)?;
        debug!("removed orphan current.seg.tmp");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::PendingRow;
    use crate::segment::SegmentWriter;

    fn setup_staging(tmp: &tempfile::TempDir) -> (std::path::PathBuf, Index) {
        let root = tmp.path().to_path_buf();
        let db_path = root.join("index.db");
        let index = Index::open(&db_path).expect("open index");
        fs::create_dir_all(root.join("segments")).expect("mkdir segments");
        (root, index)
    }

    #[test]
    fn recover_empty_staging_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let result = recover(&root, &index).expect("recover empty");
        assert!(result.is_none());
    }

    #[test]
    fn recover_truncates_current_segment_to_committed_offset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");

        // Write two chunks to the segment.
        let mut writer =
            SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");
        let data1 = vec![0xAA; 100];
        let loc1 = writer.append(&data1).expect("append 1");
        let data2 = vec![0xBB; 200];
        let _loc2 = writer.append(&data2).expect("append 2");

        // Register a file so FK is satisfied, then flush only the first chunk.
        let fh = [0xF1u8; 32];
        let fh_slice: &[u8] = &fh;
        index
            .connection()
            .execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, 100)",
                rusqlite::params![fh_slice],
            )
            .expect("insert file");

        let pending = vec![PendingRow {
            chunk_hash: [0xC1u8; 32],
            file_hash: fh,
            chunk_index: 0,
            size: i64::from(loc1.length),
            segment_id: seg_id,
            segment_offset: loc1.offset,
        }];
        index.insert_pending(&pending).expect("insert pending");
        let committed_size = loc1.offset + u64::from(loc1.length) + 8;
        index.flush_pending(seg_id, committed_size).expect("flush");

        // The second chunk was written to the segment file but never
        // registered in the index — simulating a crash between the
        // segment append and the insert_pending call.

        let file_size_before = fs::metadata(segments_dir.join("current.seg"))
            .expect("stat")
            .len();
        let committed_offset = index.max_committed_offset(seg_id).expect("offset");
        assert!(file_size_before > committed_offset);

        // Run recovery.
        drop(writer);
        let result = recover(&root, &index).expect("recover");
        let recovered = result.expect("should have recovered segment");
        assert_eq!(recovered.segment_id, seg_id);
        assert_eq!(recovered.write_offset, committed_offset);

        // File should be truncated to committed offset.
        let file_size_after = fs::metadata(segments_dir.join("current.seg"))
            .expect("stat after")
            .len();
        assert_eq!(file_size_after, committed_offset);
    }

    #[test]
    fn recover_drops_pending_row_that_crosses_recovered_boundary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");

        let mut writer =
            SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");
        let data = vec![0xAB; 100];
        let loc = writer.append(&data).expect("append");
        drop(writer);

        let fh = [0xF2u8; 32];
        let fh_slice: &[u8] = &fh;
        index
            .connection()
            .execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, 100)",
                rusqlite::params![fh_slice],
            )
            .expect("insert file");

        let pending = [PendingRow {
            chunk_hash: [0xC2u8; 32],
            file_hash: fh,
            chunk_index: 0,
            size: i64::from(loc.length),
            segment_id: seg_id,
            segment_offset: loc.offset,
        }];
        index.insert_pending(&pending).expect("insert pending");

        let partial_boundary = loc.offset + u64::from(loc.length / 2);
        index
            .flush_pending(seg_id, partial_boundary)
            .expect("flush partial");

        let recovered = recover(&root, &index)
            .expect("recover")
            .expect("current segment");

        assert_eq!(recovered.write_offset, 0);
        assert_eq!(
            fs::metadata(segments_dir.join("current.seg"))
                .expect("stat")
                .len(),
            0
        );
        assert_eq!(
            index
                .segment_pending_chunk_count(seg_id)
                .expect("pending count"),
            0
        );
        assert_eq!(
            index
                .max_committed_offset(seg_id)
                .expect("committed offset"),
            0
        );
    }

    #[test]
    fn recover_preserves_full_pending_row_before_torn_tail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");

        let mut writer =
            SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");
        let loc1 = writer.append(&[0xAB; 100]).expect("append 1");
        let loc2 = writer.append(&[0xCD; 200]).expect("append 2");
        drop(writer);

        let fh = [0xF5u8; 32];
        let fh_slice: &[u8] = &fh;
        index
            .connection()
            .execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, 300)",
                rusqlite::params![fh_slice],
            )
            .expect("insert file");

        let chunk1 = [0xC5u8; 32];
        let chunk2 = [0xC6u8; 32];
        let pending = [
            PendingRow {
                chunk_hash: chunk1,
                file_hash: fh,
                chunk_index: 0,
                size: i64::from(loc1.length),
                segment_id: seg_id,
                segment_offset: loc1.offset,
            },
            PendingRow {
                chunk_hash: chunk2,
                file_hash: fh,
                chunk_index: 1,
                size: i64::from(loc2.length),
                segment_id: seg_id,
                segment_offset: loc2.offset,
            },
        ];
        index.insert_pending(&pending).expect("insert pending");

        let first_end = loc1.offset + u64::from(loc1.length) + 8;
        let second_end = loc2.offset + u64::from(loc2.length) + 8;
        index.flush_pending(seg_id, second_end).expect("flush");

        let torn_tail_len = loc2.offset + u64::from(loc2.length / 2);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(segments_dir.join("current.seg"))
            .expect("open current");
        file.set_len(torn_tail_len).expect("truncate current");
        drop(file);

        let recovered = recover(&root, &index)
            .expect("recover")
            .expect("current segment");

        assert_eq!(recovered.write_offset, first_end);
        assert_eq!(
            fs::metadata(segments_dir.join("current.seg"))
                .expect("stat")
                .len(),
            first_end
        );
        assert_eq!(
            index
                .segment_pending_chunk_count(seg_id)
                .expect("pending count"),
            1
        );
        assert_eq!(index.chunks_for_file(&fh).expect("chunks"), vec![chunk1]);
        assert_eq!(
            index
                .max_committed_offset(seg_id)
                .expect("committed offset"),
            first_end
        );
    }

    #[test]
    fn recover_does_not_extend_short_current_segment_from_sqlite_boundary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");

        let mut writer =
            SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");
        let data = vec![0xBC; 100];
        let loc = writer.append(&data).expect("append");
        drop(writer);

        let fh = [0xF3u8; 32];
        let fh_slice: &[u8] = &fh;
        index
            .connection()
            .execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, 100)",
                rusqlite::params![fh_slice],
            )
            .expect("insert file");

        let pending = [PendingRow {
            chunk_hash: [0xC3u8; 32],
            file_hash: fh,
            chunk_index: 0,
            size: i64::from(loc.length),
            segment_id: seg_id,
            segment_offset: loc.offset,
        }];
        index.insert_pending(&pending).expect("insert pending");
        let recorded_end = loc.offset + u64::from(loc.length) + 8;
        index.flush_pending(seg_id, recorded_end).expect("flush");

        let actual_len = loc.offset + u64::from(loc.length / 2);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(segments_dir.join("current.seg"))
            .expect("open current");
        file.set_len(actual_len).expect("truncate current");
        drop(file);

        let recovered = recover(&root, &index)
            .expect("recover")
            .expect("current segment");

        assert_eq!(recovered.write_offset, 0);
        assert_eq!(
            fs::metadata(segments_dir.join("current.seg"))
                .expect("stat")
                .len(),
            0
        );
        assert_eq!(
            index
                .segment_pending_chunk_count(seg_id)
                .expect("pending count"),
            0
        );
        assert_eq!(
            index
                .max_committed_offset(seg_id)
                .expect("committed offset"),
            0
        );
    }

    #[test]
    fn recover_rejects_current_segment_shorter_than_promoted_chunk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");
        fs::write(segments_dir.join("current.seg"), vec![0u8; 50]).expect("write current");

        let fh = [0xF4u8; 32];
        let fh_slice: &[u8] = &fh;
        let ch = [0xC4u8; 32];
        let ch_slice: &[u8] = &ch;
        index
            .connection()
            .execute(
                "INSERT INTO files (file_hash, total_bytes) VALUES (?1, 100)",
                rusqlite::params![fh_slice],
            )
            .expect("insert file");
        index
            .connection()
            .execute(
                "INSERT INTO chunks
                 (chunk_hash, file_hash, chunk_index, size, segment_id, segment_offset)
                 VALUES (?1, ?2, 0, 100, ?3, 0)",
                rusqlite::params![ch_slice, fh_slice, seg_id],
            )
            .expect("insert chunk");

        let err = recover(&root, &index).expect_err("short promoted chunk must fail");

        assert!(matches!(err, StagingError::StagingCorrupt(_)));
    }

    #[test]
    fn recover_rejects_undersized_sealed_segment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");

        // Seal the segment in the index with a large size.
        index.connection().execute(
            "UPDATE segments SET sealed_at = datetime('now'), size_bytes = 1000 WHERE segment_id = ?1",
            rusqlite::params![seg_id],
        ).expect("seal");

        // Create a file that's too small.
        fs::write(segments_dir.join(format!("{seg_id:016x}.seg")), [0u8; 100])
            .expect("write small file");

        let err = recover(&root, &index).expect_err("should fail");
        assert!(matches!(err, StagingError::StagingCorrupt(_)));
    }

    #[test]
    fn recover_accepts_sealed_segment_with_sufficient_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");

        index.connection().execute(
            "UPDATE segments SET sealed_at = datetime('now'), size_bytes = 50 WHERE segment_id = ?1",
            rusqlite::params![seg_id],
        ).expect("seal");

        // Create a file that's large enough.
        fs::write(segments_dir.join(format!("{seg_id:016x}.seg")), [0u8; 100]).expect("write file");

        let result = recover(&root, &index).expect("recover should succeed");
        assert!(result.is_none(), "no current segment to recover");
    }

    #[test]
    fn recover_cleans_up_orphan_tmp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let tmp_path = segments_dir.join("current.seg.tmp");
        fs::write(&tmp_path, b"orphan data").expect("write tmp");
        assert!(tmp_path.exists());

        recover(&root, &index).expect("recover");
        assert!(!tmp_path.exists());
    }

    #[test]
    fn recover_rejects_missing_sealed_segment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);

        let seg_id = index.allocate_segment_id().expect("alloc");

        // Seal in index but don't create the file.
        index.connection().execute(
            "UPDATE segments SET sealed_at = datetime('now'), size_bytes = 100 WHERE segment_id = ?1",
            rusqlite::params![seg_id],
        ).expect("seal");

        let err = recover(&root, &index).expect_err("should fail");
        assert!(matches!(err, StagingError::StagingCorrupt(_)));
    }

    #[test]
    fn recover_current_segment_with_no_committed_chunks_truncates_to_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (root, index) = setup_staging(&tmp);
        let segments_dir = root.join("segments");

        let seg_id = index.allocate_segment_id().expect("alloc");
        index.register_current_segment(seg_id).expect("register");

        // Write some data but never flush to committed.
        let mut writer =
            SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");
        writer.append(&[0xCC; 500]).expect("append");
        drop(writer);

        let result = recover(&root, &index).expect("recover");
        let recovered = result.expect("should have recovered segment");
        assert_eq!(recovered.segment_id, seg_id);
        assert_eq!(recovered.write_offset, 0);

        // File should be truncated to 0.
        let file_size = fs::metadata(segments_dir.join("current.seg"))
            .expect("stat")
            .len();
        assert_eq!(file_size, 0);
    }
}
