//! Chunk comparison: LCS-based segment diff producing ChunkDiffReport.

use crab_xet::shard::FileDataSequenceEntry;

use crate::types::{ChunkDiffReport, FileStatus, SegmentDiff, SegmentStatus};

/// Key used for segment equality: `(xorb_hash, chunk_index_start, chunk_index_end)`.
/// `unpacked_segment_bytes` and `xorb_flags` are excluded from equality.
type SegmentKey = ([u64; 4], u32, u32);

fn segment_key(entry: &FileDataSequenceEntry) -> SegmentKey {
    (
        *entry.xorb_hash,
        entry.chunk_index_start,
        entry.chunk_index_end,
    )
}

/// Compute the LCS length table for two segment lists.
/// `dp[i][j]` = length of LCS of `old[..i]` and `new[..j]`.
fn lcs_table(old: &[FileDataSequenceEntry], new: &[FileDataSequenceEntry]) -> Vec<Vec<u32>> {
    let m = old.len();
    let n = new.len();
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if segment_key(&old[i - 1]) == segment_key(&new[j - 1]) {
                dp[i - 1][j - 1] + 1
            } else {
                dp[i - 1][j].max(dp[i][j - 1])
            };
        }
    }
    dp
}

/// Maximum combined segment count where we use the exact LCS algorithm.
/// The DP table is `(m+1) × (n+1) × 4 bytes`. At m=n=8k this is ~256 MB,
/// which is still acceptable on typical dev machines. Above this we fall
/// back to order-preserving set comparison that classifies entries as
/// unchanged/added/removed without computing the longest-common-subsequence.
/// See finding CR8-F1.
const LCS_SEGMENT_CEILING: usize = 8_192;

/// Classify segments using set membership rather than LCS.
///
/// Entries present in both old and new are marked `Unchanged`.
/// Entries only in old are `Removed`; entries only in new are `Added`.
/// This approximation loses the ability to detect "same segment moved
/// to a different position" (LCS would report both copies as unchanged,
/// set comparison reports the extra copy as removed and the new
/// position as added) but uses O(m+n) space regardless of input size.
fn classify_segments_by_set(
    old: &[FileDataSequenceEntry],
    new: &[FileDataSequenceEntry],
) -> (Vec<SegmentStatus>, Vec<SegmentStatus>) {
    let old_keys: std::collections::HashSet<SegmentKey> = old.iter().map(segment_key).collect();
    let new_keys: std::collections::HashSet<SegmentKey> = new.iter().map(segment_key).collect();

    let old_status: Vec<SegmentStatus> = old
        .iter()
        .map(|e| {
            if new_keys.contains(&segment_key(e)) {
                SegmentStatus::Unchanged
            } else {
                SegmentStatus::Removed
            }
        })
        .collect();
    let new_status: Vec<SegmentStatus> = new
        .iter()
        .map(|e| {
            if old_keys.contains(&segment_key(e)) {
                SegmentStatus::Unchanged
            } else {
                SegmentStatus::Added
            }
        })
        .collect();

    (old_status, new_status)
}

/// Backtrace the LCS table to classify each segment.
/// Returns `(old_status, new_status)` where each element is either
/// `Unchanged` or `Removed`/`Added` respectively.
fn classify_segments(
    old: &[FileDataSequenceEntry],
    new: &[FileDataSequenceEntry],
    dp: &[Vec<u32>],
) -> (Vec<SegmentStatus>, Vec<SegmentStatus>) {
    let m = old.len();
    let n = new.len();
    let mut old_status = vec![SegmentStatus::Removed; m];
    let mut new_status = vec![SegmentStatus::Added; n];

    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        if segment_key(&old[i - 1]) == segment_key(&new[j - 1]) {
            old_status[i - 1] = SegmentStatus::Unchanged;
            new_status[j - 1] = SegmentStatus::Unchanged;
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }

    (old_status, new_status)
}

/// Compare two reconstruction term lists and produce a diff report.
///
/// Uses LCS to find the optimal alignment between old and new segment lists.
/// Two entries are "equal" when `(xorb_hash, chunk_index_start, chunk_index_end)` match.
pub fn compare_terms(
    path: &str,
    old_terms: &[FileDataSequenceEntry],
    new_terms: &[FileDataSequenceEntry],
    old_size: u64,
    new_size: u64,
) -> ChunkDiffReport {
    // Handle added files (old empty).
    if old_terms.is_empty() && !new_terms.is_empty() {
        return build_added_report(path, new_terms, new_size);
    }

    // Handle deleted files (new empty).
    if !old_terms.is_empty() && new_terms.is_empty() {
        return build_deleted_report(path, old_terms, old_size);
    }

    // Handle both empty (edge case: empty file unchanged).
    if old_terms.is_empty() && new_terms.is_empty() {
        return ChunkDiffReport {
            path: path.to_owned(),
            status: FileStatus::Modified,
            old_size,
            new_size,
            unchanged_segments: 0,
            unchanged_bytes: 0,
            removed_segments: 0,
            removed_bytes: 0,
            added_segments: 0,
            added_bytes: 0,
            delta_bytes: 0,
            dedup_ratio: compute_dedup_ratio(0, old_size, new_size),
            changed_byte_ranges: Vec::new(),
            segment_details: Vec::new(),
            annotations: Vec::new(),
            chunk_metrics: None,
        };
    }

    // Choose the comparison strategy based on input size. The exact LCS
    // uses O(m × n) space, which for very large files (tens of thousands
    // of chunks) can exceed process memory. Above LCS_SEGMENT_CEILING
    // we fall back to set-based classification that uses O(m + n) space
    // at the cost of losing "moved segment" detection. See finding CR8-F1.
    let (old_status, new_status) = if old_terms.len() + new_terms.len() > LCS_SEGMENT_CEILING {
        tracing::debug!(
            path,
            old = old_terms.len(),
            new = new_terms.len(),
            threshold = LCS_SEGMENT_CEILING,
            "using set-based segment classification for large file"
        );
        classify_segments_by_set(old_terms, new_terms)
    } else {
        let dp = lcs_table(old_terms, new_terms);
        classify_segments(old_terms, new_terms, &dp)
    };

    // Accumulate byte counts.
    let mut unchanged_bytes: u64 = 0;
    let mut removed_bytes: u64 = 0;
    let mut added_bytes: u64 = 0;
    let mut unchanged_segments: u32 = 0;
    let mut removed_segments: u32 = 0;
    let mut added_segments: u32 = 0;

    for (entry, status) in old_terms.iter().zip(old_status.iter()) {
        let bytes = u64::from(entry.unpacked_segment_bytes);
        match status {
            SegmentStatus::Unchanged => {
                unchanged_bytes += bytes;
                unchanged_segments += 1;
            }
            SegmentStatus::Removed => {
                removed_bytes += bytes;
                removed_segments += 1;
            }
            SegmentStatus::Added => unreachable!(),
        }
    }

    for (entry, status) in new_terms.iter().zip(new_status.iter()) {
        let bytes = u64::from(entry.unpacked_segment_bytes);
        match status {
            SegmentStatus::Added => {
                added_bytes += bytes;
                added_segments += 1;
            }
            SegmentStatus::Unchanged => {
                // Already counted from old side.
            }
            SegmentStatus::Removed => unreachable!(),
        }
    }

    let delta_bytes = added_bytes;
    let dedup_ratio = compute_dedup_ratio(unchanged_bytes, old_size, new_size);

    // Compute changed byte ranges from the new-side segment positions.
    let changed_byte_ranges = compute_changed_byte_ranges(new_terms, &new_status);

    // Build segment details for verbose output.
    let segment_details = build_segment_details(old_terms, &old_status, new_terms, &new_status);

    ChunkDiffReport {
        path: path.to_owned(),
        status: FileStatus::Modified,
        old_size,
        new_size,
        unchanged_segments,
        unchanged_bytes,
        removed_segments,
        removed_bytes,
        added_segments,
        added_bytes,
        delta_bytes,
        dedup_ratio,
        changed_byte_ranges,
        segment_details,
        annotations: Vec::new(),
        chunk_metrics: None,
    }
}

fn build_added_report(
    path: &str,
    new_terms: &[FileDataSequenceEntry],
    new_size: u64,
) -> ChunkDiffReport {
    let added_bytes: u64 = new_terms
        .iter()
        .map(|e| u64::from(e.unpacked_segment_bytes))
        .sum();
    let segment_details: Vec<SegmentDiff> = new_terms
        .iter()
        .enumerate()
        .map(|(i, e)| SegmentDiff {
            index: i as u32,
            status: SegmentStatus::Added,
            old_xorb_hash: None,
            new_xorb_hash: Some(e.xorb_hash.to_string()),
            old_chunk_range: None,
            new_chunk_range: Some((e.chunk_index_start, e.chunk_index_end)),
            bytes: u64::from(e.unpacked_segment_bytes),
        })
        .collect();

    ChunkDiffReport {
        path: path.to_owned(),
        status: FileStatus::Added,
        old_size: 0,
        new_size,
        unchanged_segments: 0,
        unchanged_bytes: 0,
        removed_segments: 0,
        removed_bytes: 0,
        added_segments: new_terms.len() as u32,
        added_bytes,
        delta_bytes: added_bytes,
        dedup_ratio: 0.0,
        changed_byte_ranges: Vec::new(),
        segment_details,
        annotations: Vec::new(),
        chunk_metrics: None,
    }
}

fn build_deleted_report(
    path: &str,
    old_terms: &[FileDataSequenceEntry],
    old_size: u64,
) -> ChunkDiffReport {
    let removed_bytes: u64 = old_terms
        .iter()
        .map(|e| u64::from(e.unpacked_segment_bytes))
        .sum();
    let segment_details: Vec<SegmentDiff> = old_terms
        .iter()
        .enumerate()
        .map(|(i, e)| SegmentDiff {
            index: i as u32,
            status: SegmentStatus::Removed,
            old_xorb_hash: Some(e.xorb_hash.to_string()),
            new_xorb_hash: None,
            old_chunk_range: Some((e.chunk_index_start, e.chunk_index_end)),
            new_chunk_range: None,
            bytes: u64::from(e.unpacked_segment_bytes),
        })
        .collect();

    ChunkDiffReport {
        path: path.to_owned(),
        status: FileStatus::Deleted,
        old_size,
        new_size: 0,
        unchanged_segments: 0,
        unchanged_bytes: 0,
        removed_segments: old_terms.len() as u32,
        removed_bytes,
        added_segments: 0,
        added_bytes: 0,
        delta_bytes: 0,
        dedup_ratio: 0.0,
        changed_byte_ranges: Vec::new(),
        segment_details,
        annotations: Vec::new(),
        chunk_metrics: None,
    }
}

fn compute_dedup_ratio(unchanged_bytes: u64, old_size: u64, new_size: u64) -> f64 {
    let max_size = old_size.max(new_size);
    if max_size == 0 {
        return 0.0;
    }
    unchanged_bytes as f64 / max_size as f64
}

/// Compute changed byte ranges from segment positions in the new file.
/// Walks through new-side segments, accumulating byte offsets. Contiguous
/// changed (Added) regions are merged into `(offset, length)` pairs.
fn compute_changed_byte_ranges(
    new_terms: &[FileDataSequenceEntry],
    new_status: &[SegmentStatus],
) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut offset: u64 = 0;
    let mut current_start: Option<u64> = None;
    let mut current_len: u64 = 0;

    for (entry, status) in new_terms.iter().zip(new_status.iter()) {
        let bytes = u64::from(entry.unpacked_segment_bytes);
        match status {
            SegmentStatus::Added => {
                if current_start.is_none() {
                    current_start = Some(offset);
                    current_len = 0;
                }
                current_len += bytes;
            }
            SegmentStatus::Unchanged => {
                if let Some(start) = current_start.take() {
                    ranges.push((start, current_len));
                    current_len = 0;
                }
            }
            SegmentStatus::Removed => unreachable!(),
        }
        offset += bytes;
    }

    // Flush trailing changed region.
    if let Some(start) = current_start {
        ranges.push((start, current_len));
    }

    ranges
}

/// Build per-segment detail records for verbose output.
fn build_segment_details(
    old_terms: &[FileDataSequenceEntry],
    old_status: &[SegmentStatus],
    new_terms: &[FileDataSequenceEntry],
    new_status: &[SegmentStatus],
) -> Vec<SegmentDiff> {
    let mut details = Vec::new();
    let mut old_idx = 0;
    let mut new_idx = 0;

    // Interleave old and new segments in order, pairing unchanged segments.
    while old_idx < old_terms.len() || new_idx < new_terms.len() {
        // Emit removed segments from old side.
        if old_idx < old_terms.len() && old_status[old_idx] == SegmentStatus::Removed {
            let e = &old_terms[old_idx];
            details.push(SegmentDiff {
                index: old_idx as u32,
                status: SegmentStatus::Removed,
                old_xorb_hash: Some(e.xorb_hash.to_string()),
                new_xorb_hash: None,
                old_chunk_range: Some((e.chunk_index_start, e.chunk_index_end)),
                new_chunk_range: None,
                bytes: u64::from(e.unpacked_segment_bytes),
            });
            old_idx += 1;
            continue;
        }

        // Emit added segments from new side.
        if new_idx < new_terms.len() && new_status[new_idx] == SegmentStatus::Added {
            let e = &new_terms[new_idx];
            details.push(SegmentDiff {
                index: new_idx as u32,
                status: SegmentStatus::Added,
                old_xorb_hash: None,
                new_xorb_hash: Some(e.xorb_hash.to_string()),
                old_chunk_range: None,
                new_chunk_range: Some((e.chunk_index_start, e.chunk_index_end)),
                bytes: u64::from(e.unpacked_segment_bytes),
            });
            new_idx += 1;
            continue;
        }

        // Both are unchanged — emit as unchanged and advance both.
        if old_idx < old_terms.len() && new_idx < new_terms.len() {
            let old_e = &old_terms[old_idx];
            let new_e = &new_terms[new_idx];
            details.push(SegmentDiff {
                index: new_idx as u32,
                status: SegmentStatus::Unchanged,
                old_xorb_hash: Some(old_e.xorb_hash.to_string()),
                new_xorb_hash: Some(new_e.xorb_hash.to_string()),
                old_chunk_range: Some((old_e.chunk_index_start, old_e.chunk_index_end)),
                new_chunk_range: Some((new_e.chunk_index_start, new_e.chunk_index_end)),
                bytes: u64::from(new_e.unpacked_segment_bytes),
            });
            old_idx += 1;
            new_idx += 1;
            continue;
        }

        // Shouldn't reach here, but advance to avoid infinite loop.
        old_idx += 1;
        new_idx += 1;
    }

    details
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_xet::hash::MerkleHash;

    fn make_entry(hash_seed: u64, start: u32, end: u32, bytes: u32) -> FileDataSequenceEntry {
        FileDataSequenceEntry::new(
            MerkleHash::from([hash_seed, hash_seed, hash_seed, hash_seed]),
            bytes,
            start,
            end,
        )
    }

    #[test]
    fn identical_lists_report_zero_changes() {
        let terms = vec![make_entry(1, 0, 3, 1000), make_entry(2, 0, 5, 2000)];
        let report = compare_terms("test.bin", &terms, &terms, 3000, 3000);

        assert_eq!(report.unchanged_segments, 2);
        assert_eq!(report.removed_segments, 0);
        assert_eq!(report.added_segments, 0);
        assert_eq!(report.unchanged_bytes, 3000);
        assert_eq!(report.removed_bytes, 0);
        assert_eq!(report.added_bytes, 0);
        assert_eq!(report.delta_bytes, 0);
        assert_eq!(report.status, FileStatus::Modified);
        assert!(report.changed_byte_ranges.is_empty());
    }

    #[test]
    fn added_file_all_segments_added() {
        let new_terms = vec![make_entry(1, 0, 3, 1000), make_entry(2, 0, 5, 2000)];
        let report = compare_terms("new.bin", &[], &new_terms, 0, 3000);

        assert_eq!(report.status, FileStatus::Added);
        assert_eq!(report.added_segments, 2);
        assert_eq!(report.added_bytes, 3000);
        assert_eq!(report.delta_bytes, 3000);
        assert_eq!(report.unchanged_segments, 0);
        assert_eq!(report.removed_segments, 0);
        assert_eq!(report.dedup_ratio, 0.0);
    }

    #[test]
    fn deleted_file_all_segments_removed() {
        let old_terms = vec![make_entry(1, 0, 3, 1000), make_entry(2, 0, 5, 2000)];
        let report = compare_terms("old.bin", &old_terms, &[], 3000, 0);

        assert_eq!(report.status, FileStatus::Deleted);
        assert_eq!(report.removed_segments, 2);
        assert_eq!(report.removed_bytes, 3000);
        assert_eq!(report.delta_bytes, 0);
        assert_eq!(report.unchanged_segments, 0);
        assert_eq!(report.added_segments, 0);
        assert_eq!(report.dedup_ratio, 0.0);
    }

    #[test]
    fn one_segment_changed_in_middle() {
        let old = vec![
            make_entry(1, 0, 3, 1000),
            make_entry(2, 0, 5, 2000),
            make_entry(3, 0, 2, 500),
        ];
        let new = vec![
            make_entry(1, 0, 3, 1000),
            make_entry(99, 0, 5, 2500),
            make_entry(3, 0, 2, 500),
        ];
        let report = compare_terms("mod.bin", &old, &new, 3500, 4000);

        assert_eq!(report.unchanged_segments, 2);
        assert_eq!(report.removed_segments, 1);
        assert_eq!(report.added_segments, 1);
        assert_eq!(report.unchanged_bytes, 1500);
        assert_eq!(report.removed_bytes, 2000);
        assert_eq!(report.added_bytes, 2500);
        assert_eq!(report.delta_bytes, 2500);
        assert_eq!(report.changed_byte_ranges, vec![(1000, 2500)]);
    }

    #[test]
    fn segment_counts_add_up() {
        let old = vec![make_entry(1, 0, 3, 1000), make_entry(2, 0, 5, 2000)];
        let new = vec![
            make_entry(1, 0, 3, 1000),
            make_entry(3, 0, 5, 3000),
            make_entry(4, 0, 2, 500),
        ];
        let report = compare_terms("test.bin", &old, &new, 3000, 4500);

        assert_eq!(
            report.unchanged_segments + report.removed_segments,
            old.len() as u32
        );
        assert_eq!(
            report.unchanged_segments + report.added_segments,
            new.len() as u32
        );
    }

    #[test]
    fn byte_sums_consistent() {
        let old = vec![make_entry(1, 0, 3, 1000), make_entry(2, 0, 5, 2000)];
        let new = vec![make_entry(1, 0, 3, 1000), make_entry(3, 0, 5, 3000)];
        let old_total: u64 = old.iter().map(|e| e.unpacked_segment_bytes as u64).sum();
        let new_total: u64 = new.iter().map(|e| e.unpacked_segment_bytes as u64).sum();
        let report = compare_terms("test.bin", &old, &new, old_total, new_total);

        assert_eq!(report.unchanged_bytes + report.removed_bytes, old_total);
        assert_eq!(report.unchanged_bytes + report.added_bytes, new_total);
        assert_eq!(report.delta_bytes, report.added_bytes);
    }

    #[test]
    fn dedup_ratio_computed_correctly() {
        let old = vec![make_entry(1, 0, 3, 8000)];
        let new = vec![make_entry(1, 0, 3, 8000), make_entry(2, 0, 5, 2000)];
        let report = compare_terms("test.bin", &old, &new, 8000, 10000);

        // unchanged_bytes = 8000, max(8000, 10000) = 10000
        assert!((report.dedup_ratio - 0.8).abs() < 1e-10);
    }

    #[test]
    fn both_empty_reports_no_changes() {
        let report = compare_terms("empty.bin", &[], &[], 0, 0);

        assert_eq!(report.unchanged_segments, 0);
        assert_eq!(report.removed_segments, 0);
        assert_eq!(report.added_segments, 0);
        assert_eq!(report.delta_bytes, 0);
    }

    #[test]
    fn contiguous_changed_ranges_merged() {
        let old = vec![
            make_entry(1, 0, 1, 100),
            make_entry(2, 0, 1, 200),
            make_entry(3, 0, 1, 300),
            make_entry(4, 0, 1, 400),
        ];
        // Change middle two segments.
        let new = vec![
            make_entry(1, 0, 1, 100),
            make_entry(20, 0, 1, 200),
            make_entry(30, 0, 1, 300),
            make_entry(4, 0, 1, 400),
        ];
        let report = compare_terms("test.bin", &old, &new, 1000, 1000);

        // Two contiguous changed segments should merge into one range.
        assert_eq!(report.changed_byte_ranges, vec![(100, 500)]);
    }
}
