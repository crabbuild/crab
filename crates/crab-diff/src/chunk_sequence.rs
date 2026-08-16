//! Chunk-sequence diffing for Crab-managed large files.
//!
//! Reconstruction terms identify ranges within xorbs. For diffs we compare
//! the actual ordered chunk hashes so reused content is recognized even when
//! packing moved it to a different xorb or xorb offset.

use std::collections::{HashMap, VecDeque};

use crab_xet::hash::MerkleHash;
use tracing::debug;

use crate::types::{
    ChunkDiffMetrics, ChunkDiffReport, ChunkSequenceSourceKind, FileStatus, SegmentDiff,
    SegmentStatus,
};

/// Metadata for where a chunk came from, when the source has that context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkOrigin {
    pub xorb_hash: Option<MerkleHash>,
    pub xorb_chunk_index: Option<u32>,
}

/// A single file chunk with its file byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSpan {
    pub chunk_hash: MerkleHash,
    pub offset: u64,
    pub len: u64,
    pub origin: ChunkOrigin,
}

/// Ordered chunk sequence for one file version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSequence {
    pub source: ChunkSequenceSourceKind,
    pub file_hash: MerkleHash,
    pub file_size: u64,
    pub spans: Vec<ChunkSpan>,
}

impl ChunkSequence {
    /// Build a sequence from local staging chunk hashes and sizes.
    #[must_use]
    pub fn from_staged(
        file_hash: MerkleHash,
        file_size: u64,
        chunks: &[(MerkleHash, u64)],
    ) -> Self {
        let mut offset = 0u64;
        let spans = chunks
            .iter()
            .map(|&(chunk_hash, len)| {
                let span = ChunkSpan {
                    chunk_hash,
                    offset,
                    len,
                    origin: ChunkOrigin {
                        xorb_hash: None,
                        xorb_chunk_index: None,
                    },
                };
                offset = offset.saturating_add(len);
                span
            })
            .collect();

        Self {
            source: ChunkSequenceSourceKind::Staged,
            file_hash,
            file_size,
            spans,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MatchNode {
    old_idx: usize,
    new_idx: usize,
    prev: Option<usize>,
}

const EXACT_MATCH_PAIR_CEILING: usize = 2_000_000;

/// Compare two chunk sequences and produce a diff report.
#[must_use]
pub fn compare_sequences(path: &str, old: &ChunkSequence, new: &ChunkSequence) -> ChunkDiffReport {
    let old_size = old.file_size;
    let new_size = new.file_size;

    if old.spans.is_empty() && !new.spans.is_empty() {
        return build_added_report(path, old, new);
    }
    if !old.spans.is_empty() && new.spans.is_empty() {
        return build_deleted_report(path, old, new);
    }

    let matches = lcs_matches(&old.spans, &new.spans);
    let mut old_status = vec![SegmentStatus::Removed; old.spans.len()];
    let mut new_status = vec![SegmentStatus::Added; new.spans.len()];
    for (old_idx, new_idx) in matches {
        old_status[old_idx] = SegmentStatus::Unchanged;
        new_status[new_idx] = SegmentStatus::Unchanged;
    }

    let metrics = build_metrics(old, new, &old_status, &new_status);
    let segment_details = build_chunk_details(&old.spans, &old_status, &new.spans, &new_status);
    report_from_metrics(
        path,
        FileStatus::Modified,
        metrics,
        segment_details,
        old_size,
        new_size,
    )
}

fn build_added_report(path: &str, old: &ChunkSequence, new: &ChunkSequence) -> ChunkDiffReport {
    let new_status = vec![SegmentStatus::Added; new.spans.len()];
    let old_status = Vec::new();
    let metrics = build_metrics(old, new, &old_status, &new_status);
    let segment_details = build_chunk_details(&[], &old_status, &new.spans, &new_status);
    report_from_metrics(
        path,
        FileStatus::Added,
        metrics,
        segment_details,
        0,
        new.file_size,
    )
}

fn build_deleted_report(path: &str, old: &ChunkSequence, new: &ChunkSequence) -> ChunkDiffReport {
    let old_status = vec![SegmentStatus::Removed; old.spans.len()];
    let new_status = Vec::new();
    let metrics = build_metrics(old, new, &old_status, &new_status);
    let segment_details = build_chunk_details(&old.spans, &old_status, &[], &new_status);
    report_from_metrics(
        path,
        FileStatus::Deleted,
        metrics,
        segment_details,
        old.file_size,
        0,
    )
}

fn report_from_metrics(
    path: &str,
    status: FileStatus,
    metrics: ChunkDiffMetrics,
    segment_details: Vec<SegmentDiff>,
    old_size: u64,
    new_size: u64,
) -> ChunkDiffReport {
    ChunkDiffReport {
        path: path.to_owned(),
        status,
        old_size,
        new_size,
        unchanged_segments: metrics.unchanged_chunks,
        unchanged_bytes: metrics.unchanged_bytes,
        removed_segments: metrics.removed_chunks,
        removed_bytes: metrics.removed_bytes,
        added_segments: metrics.added_chunks,
        added_bytes: metrics.added_bytes,
        delta_bytes: metrics.added_bytes,
        dedup_ratio: metrics.reuse_ratio,
        changed_byte_ranges: metrics.changed_byte_ranges_new.clone(),
        segment_details,
        annotations: Vec::new(),
        chunk_metrics: Some(metrics),
    }
}

fn build_metrics(
    old: &ChunkSequence,
    new: &ChunkSequence,
    old_status: &[SegmentStatus],
    new_status: &[SegmentStatus],
) -> ChunkDiffMetrics {
    let unchanged_bytes = old
        .spans
        .iter()
        .zip(old_status)
        .filter_map(|(span, status)| (*status == SegmentStatus::Unchanged).then_some(span.len))
        .sum();
    let removed_bytes = old
        .spans
        .iter()
        .zip(old_status)
        .filter_map(|(span, status)| (*status == SegmentStatus::Removed).then_some(span.len))
        .sum();
    let added_bytes = new
        .spans
        .iter()
        .zip(new_status)
        .filter_map(|(span, status)| (*status == SegmentStatus::Added).then_some(span.len))
        .sum();

    ChunkDiffMetrics {
        old_source: old.source,
        new_source: new.source,
        old_chunks: saturating_u32(old.spans.len()),
        new_chunks: saturating_u32(new.spans.len()),
        unchanged_chunks: saturating_u32(
            old_status
                .iter()
                .filter(|&&status| status == SegmentStatus::Unchanged)
                .count(),
        ),
        removed_chunks: saturating_u32(
            old_status
                .iter()
                .filter(|&&status| status == SegmentStatus::Removed)
                .count(),
        ),
        added_chunks: saturating_u32(
            new_status
                .iter()
                .filter(|&&status| status == SegmentStatus::Added)
                .count(),
        ),
        old_bytes: old.file_size,
        new_bytes: new.file_size,
        unchanged_bytes,
        removed_bytes,
        added_bytes,
        signed_delta_bytes: signed_delta(old.file_size, new.file_size),
        reuse_ratio: compute_reuse_ratio(unchanged_bytes, old.file_size, new.file_size),
        changed_byte_ranges_old: changed_ranges(&old.spans, old_status, SegmentStatus::Removed),
        changed_byte_ranges_new: changed_ranges(&new.spans, new_status, SegmentStatus::Added),
    }
}

fn lcs_matches(old: &[ChunkSpan], new: &[ChunkSpan]) -> Vec<(usize, usize)> {
    if old.is_empty() || new.is_empty() {
        return Vec::new();
    }

    let mut prefix_len = 0usize;
    while prefix_len < old.len()
        && prefix_len < new.len()
        && old[prefix_len].chunk_hash == new[prefix_len].chunk_hash
    {
        prefix_len += 1;
    }

    let mut old_suffix_start = old.len();
    let mut new_suffix_start = new.len();
    while old_suffix_start > prefix_len
        && new_suffix_start > prefix_len
        && old[old_suffix_start - 1].chunk_hash == new[new_suffix_start - 1].chunk_hash
    {
        old_suffix_start -= 1;
        new_suffix_start -= 1;
    }

    let mut matches = Vec::with_capacity(prefix_len + old.len().saturating_sub(old_suffix_start));
    matches.extend((0..prefix_len).map(|idx| (idx, idx)));

    let middle_matches = unanchored_lcs_matches(
        &old[prefix_len..old_suffix_start],
        &new[prefix_len..new_suffix_start],
    );
    matches.extend(
        middle_matches
            .into_iter()
            .map(|(old_idx, new_idx)| (old_idx + prefix_len, new_idx + prefix_len)),
    );

    let suffix_len = old.len().saturating_sub(old_suffix_start);
    matches.extend((0..suffix_len).map(|idx| (old_suffix_start + idx, new_suffix_start + idx)));
    matches
}

fn unanchored_lcs_matches(old: &[ChunkSpan], new: &[ChunkSpan]) -> Vec<(usize, usize)> {
    if old.is_empty() || new.is_empty() || !has_shared_chunk(old, new) {
        return Vec::new();
    }

    let potential_matches = potential_match_pairs(old, new);
    if potential_matches <= EXACT_MATCH_PAIR_CEILING {
        return hunt_szymanski_lcs(old, new);
    }

    debug!(
        old_chunks = old.len(),
        new_chunks = new.len(),
        potential_matches,
        ceiling = EXACT_MATCH_PAIR_CEILING,
        "using bounded greedy chunk sequence diff for highly repetitive file"
    );
    greedy_ordered_matches(old, new)
}

fn has_shared_chunk(old: &[ChunkSpan], new: &[ChunkSpan]) -> bool {
    let old_hashes: std::collections::HashSet<MerkleHash> =
        old.iter().map(|span| span.chunk_hash).collect();
    new.iter().any(|span| old_hashes.contains(&span.chunk_hash))
}

fn potential_match_pairs(old: &[ChunkSpan], new: &[ChunkSpan]) -> usize {
    let mut old_counts: HashMap<MerkleHash, usize> = HashMap::new();
    for span in old {
        *old_counts.entry(span.chunk_hash).or_insert(0) += 1;
    }

    let mut total = 0usize;
    for span in new {
        if let Some(count) = old_counts.get(&span.chunk_hash) {
            total = total.saturating_add(*count);
            if total > EXACT_MATCH_PAIR_CEILING {
                return total;
            }
        }
    }
    total
}

fn hunt_szymanski_lcs(old: &[ChunkSpan], new: &[ChunkSpan]) -> Vec<(usize, usize)> {
    let mut new_positions: HashMap<MerkleHash, Vec<usize>> = HashMap::new();
    for (idx, span) in new.iter().enumerate() {
        new_positions.entry(span.chunk_hash).or_default().push(idx);
    }

    let mut tails: Vec<usize> = Vec::new();
    let mut tail_nodes: Vec<Option<usize>> = Vec::new();
    let mut nodes: Vec<MatchNode> = Vec::new();

    for (old_idx, span) in old.iter().enumerate() {
        let Some(positions) = new_positions.get(&span.chunk_hash) else {
            continue;
        };

        for &new_idx in positions.iter().rev() {
            let length_idx = lower_bound(&tails, new_idx);
            let prev = if length_idx == 0 {
                None
            } else {
                tail_nodes[length_idx - 1]
            };
            nodes.push(MatchNode {
                old_idx,
                new_idx,
                prev,
            });
            let node_idx = Some(nodes.len() - 1);

            if length_idx == tails.len() {
                tails.push(new_idx);
                tail_nodes.push(node_idx);
            } else {
                tails[length_idx] = new_idx;
                tail_nodes[length_idx] = node_idx;
            }
        }
    }

    let mut matches = Vec::with_capacity(tails.len());
    let mut node_idx = tail_nodes.last().copied().flatten();
    while let Some(idx) = node_idx {
        let node = nodes[idx];
        matches.push((node.old_idx, node.new_idx));
        node_idx = node.prev;
    }
    matches.reverse();
    matches
}

fn lower_bound(items: &[usize], target: usize) -> usize {
    let mut left = 0usize;
    let mut right = items.len();
    while left < right {
        let mid = left + (right - left) / 2;
        if items[mid] < target {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    left
}

fn greedy_ordered_matches(old: &[ChunkSpan], new: &[ChunkSpan]) -> Vec<(usize, usize)> {
    let mut positions: HashMap<MerkleHash, VecDeque<usize>> = HashMap::new();
    for (idx, span) in new.iter().enumerate() {
        positions.entry(span.chunk_hash).or_default().push_back(idx);
    }

    let mut matches = Vec::new();
    let mut next_allowed = 0usize;
    for (old_idx, span) in old.iter().enumerate() {
        let Some(queue) = positions.get_mut(&span.chunk_hash) else {
            continue;
        };
        while let Some(&front) = queue.front() {
            if front < next_allowed {
                queue.pop_front();
            } else {
                break;
            }
        }
        if let Some(new_idx) = queue.pop_front() {
            matches.push((old_idx, new_idx));
            next_allowed = new_idx.saturating_add(1);
        }
    }
    matches
}

fn changed_ranges(
    spans: &[ChunkSpan],
    statuses: &[SegmentStatus],
    changed_status: SegmentStatus,
) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut current_start: Option<u64> = None;
    let mut current_end = 0u64;

    for (span, status) in spans.iter().zip(statuses) {
        if *status == changed_status {
            if current_start.is_none() {
                current_start = Some(span.offset);
            }
            current_end = span.offset.saturating_add(span.len);
            continue;
        }

        if let Some(start) = current_start.take() {
            ranges.push((start, current_end.saturating_sub(start)));
        }
    }

    if let Some(start) = current_start {
        ranges.push((start, current_end.saturating_sub(start)));
    }

    ranges
}

fn build_chunk_details(
    old: &[ChunkSpan],
    old_status: &[SegmentStatus],
    new: &[ChunkSpan],
    new_status: &[SegmentStatus],
) -> Vec<SegmentDiff> {
    let mut details = Vec::with_capacity(old.len().saturating_add(new.len()));
    let mut old_idx = 0usize;
    let mut new_idx = 0usize;

    while old_idx < old.len() || new_idx < new.len() {
        if old_idx < old.len() && old_status[old_idx] == SegmentStatus::Removed {
            let span = old[old_idx];
            details.push(chunk_detail(
                old_idx,
                SegmentStatus::Removed,
                Some(span),
                None,
            ));
            old_idx += 1;
            continue;
        }

        if new_idx < new.len() && new_status[new_idx] == SegmentStatus::Added {
            let span = new[new_idx];
            details.push(chunk_detail(
                new_idx,
                SegmentStatus::Added,
                None,
                Some(span),
            ));
            new_idx += 1;
            continue;
        }

        if old_idx < old.len() && new_idx < new.len() {
            details.push(chunk_detail(
                new_idx,
                SegmentStatus::Unchanged,
                Some(old[old_idx]),
                Some(new[new_idx]),
            ));
            old_idx += 1;
            new_idx += 1;
            continue;
        }

        old_idx = old_idx.saturating_add(1);
        new_idx = new_idx.saturating_add(1);
    }

    details
}

fn chunk_detail(
    index: usize,
    status: SegmentStatus,
    old: Option<ChunkSpan>,
    new: Option<ChunkSpan>,
) -> SegmentDiff {
    SegmentDiff {
        index: saturating_u32(index),
        status,
        old_xorb_hash: old.map(display_origin_hash),
        new_xorb_hash: new.map(display_origin_hash),
        old_chunk_range: old.and_then(chunk_range),
        new_chunk_range: new.and_then(chunk_range),
        bytes: new.or(old).map_or(0, |span| span.len),
    }
}

fn display_origin_hash(span: ChunkSpan) -> String {
    span.origin.xorb_hash.unwrap_or(span.chunk_hash).to_string()
}

fn chunk_range(span: ChunkSpan) -> Option<(u32, u32)> {
    span.origin
        .xorb_chunk_index
        .map(|idx| (idx, idx.saturating_add(1)))
}

fn compute_reuse_ratio(unchanged_bytes: u64, old_size: u64, new_size: u64) -> f64 {
    let max_size = old_size.max(new_size);
    if max_size == 0 {
        return 0.0;
    }
    unchanged_bytes as f64 / max_size as f64
}

fn signed_delta(old_size: u64, new_size: u64) -> i64 {
    let delta = i128::from(new_size) - i128::from(old_size);
    delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> MerkleHash {
        [seed; 32].into()
    }

    fn sequence(source: ChunkSequenceSourceKind, chunks: &[(u8, u64)]) -> ChunkSequence {
        let mut offset = 0u64;
        let spans = chunks
            .iter()
            .enumerate()
            .map(|(idx, &(seed, len))| {
                let span = ChunkSpan {
                    chunk_hash: hash(seed),
                    offset,
                    len,
                    origin: ChunkOrigin {
                        xorb_hash: Some(hash(seed.wrapping_add(100))),
                        xorb_chunk_index: Some(idx as u32),
                    },
                };
                offset += len;
                span
            })
            .collect();
        ChunkSequence {
            source,
            file_hash: hash(42),
            file_size: offset,
            spans,
        }
    }

    #[test]
    fn same_chunk_hashes_are_unchanged_across_different_xorb_origins() {
        let mut old = sequence(ChunkSequenceSourceKind::Committed, &[(1, 10), (2, 20)]);
        let mut new = sequence(ChunkSequenceSourceKind::Committed, &[(1, 10), (2, 20)]);
        old.spans[0].origin.xorb_hash = Some(hash(10));
        new.spans[0].origin.xorb_hash = Some(hash(20));

        let report = compare_sequences("model.bin", &old, &new);

        assert_eq!(report.unchanged_segments, 2);
        assert_eq!(report.added_segments, 0);
        assert_eq!(report.removed_segments, 0);
        assert_eq!(report.unchanged_bytes, 30);
        assert_eq!(report.chunk_metrics.unwrap().reuse_ratio, 1.0);
    }

    #[test]
    fn inserted_chunk_reports_new_byte_range() {
        let old = sequence(ChunkSequenceSourceKind::Committed, &[(1, 10), (3, 30)]);
        let new = sequence(
            ChunkSequenceSourceKind::Committed,
            &[(1, 10), (2, 20), (3, 30)],
        );

        let report = compare_sequences("model.bin", &old, &new);

        assert_eq!(report.unchanged_segments, 2);
        assert_eq!(report.added_segments, 1);
        assert_eq!(report.added_bytes, 20);
        assert_eq!(report.changed_byte_ranges, vec![(10, 20)]);
        let metrics = report.chunk_metrics.unwrap();
        assert_eq!(metrics.changed_byte_ranges_new, vec![(10, 20)]);
        assert_eq!(metrics.changed_byte_ranges_old, Vec::<(u64, u64)>::new());
    }

    #[test]
    fn deleted_chunk_reports_old_byte_range() {
        let old = sequence(
            ChunkSequenceSourceKind::Committed,
            &[(1, 10), (2, 20), (3, 30)],
        );
        let new = sequence(ChunkSequenceSourceKind::Committed, &[(1, 10), (3, 30)]);

        let report = compare_sequences("model.bin", &old, &new);

        assert_eq!(report.unchanged_segments, 2);
        assert_eq!(report.removed_segments, 1);
        assert_eq!(report.removed_bytes, 20);
        let metrics = report.chunk_metrics.unwrap();
        assert_eq!(metrics.changed_byte_ranges_old, vec![(10, 20)]);
        assert_eq!(metrics.changed_byte_ranges_new, Vec::<(u64, u64)>::new());
    }

    #[test]
    fn replacement_reports_old_and_new_byte_ranges() {
        let old = sequence(
            ChunkSequenceSourceKind::Committed,
            &[(1, 10), (2, 20), (3, 30)],
        );
        let new = sequence(
            ChunkSequenceSourceKind::Committed,
            &[(1, 10), (4, 25), (3, 30)],
        );

        let report = compare_sequences("model.bin", &old, &new);
        let metrics = report.chunk_metrics.unwrap();

        assert_eq!(metrics.unchanged_chunks, 2);
        assert_eq!(metrics.removed_chunks, 1);
        assert_eq!(metrics.added_chunks, 1);
        assert_eq!(metrics.signed_delta_bytes, 5);
        assert_eq!(metrics.changed_byte_ranges_old, vec![(10, 20)]);
        assert_eq!(metrics.changed_byte_ranges_new, vec![(10, 25)]);
    }

    #[test]
    fn repeated_chunks_preserve_sequence_order() {
        let old = sequence(
            ChunkSequenceSourceKind::Committed,
            &[(1, 10), (2, 10), (1, 10), (3, 10)],
        );
        let new = sequence(
            ChunkSequenceSourceKind::Committed,
            &[(1, 10), (1, 10), (3, 10)],
        );

        let report = compare_sequences("model.bin", &old, &new);

        assert_eq!(report.unchanged_segments, 3);
        assert_eq!(report.removed_segments, 1);
        assert_eq!(report.removed_bytes, 10);
    }

    #[test]
    fn repeated_replacement_preserves_same_position_ranges() {
        let mut old_chunks = Vec::new();
        old_chunks.extend([(1, 10), (2, 10)]);
        old_chunks.extend([(1, 10), (2, 10)]);
        old_chunks.extend([(1, 10), (2, 10)]);

        let mut new_chunks = Vec::new();
        new_chunks.extend([(1, 10), (2, 10)]);
        new_chunks.extend([(9, 10), (10, 10)]);
        new_chunks.extend([(1, 10), (2, 10)]);

        let old = sequence(ChunkSequenceSourceKind::Committed, &old_chunks);
        let new = sequence(ChunkSequenceSourceKind::Committed, &new_chunks);

        let report = compare_sequences("model.bin", &old, &new);
        let metrics = report.chunk_metrics.unwrap();

        assert_eq!(metrics.unchanged_chunks, 4);
        assert_eq!(metrics.removed_chunks, 2);
        assert_eq!(metrics.added_chunks, 2);
        assert_eq!(metrics.changed_byte_ranges_old, vec![(20, 20)]);
        assert_eq!(metrics.changed_byte_ranges_new, vec![(20, 20)]);
    }

    #[test]
    fn reordered_chunks_are_not_treated_as_plain_set_reuse() {
        let old = sequence(ChunkSequenceSourceKind::Committed, &[(1, 10), (2, 10)]);
        let new = sequence(ChunkSequenceSourceKind::Committed, &[(2, 10), (1, 10)]);

        let report = compare_sequences("model.bin", &old, &new);
        let metrics = report.chunk_metrics.unwrap();

        assert_eq!(metrics.unchanged_chunks, 1);
        assert_eq!(metrics.removed_chunks, 1);
        assert_eq!(metrics.added_chunks, 1);
        assert_eq!(metrics.changed_byte_ranges_old, vec![(0, 10)]);
        assert_eq!(metrics.changed_byte_ranges_new, vec![(10, 10)]);
    }

    #[test]
    fn highly_repetitive_sequences_use_bounded_fallback() {
        let mut old_chunks = vec![(1, 1)];
        old_chunks.extend(vec![(7, 1); 1_500]);
        old_chunks.push((2, 1));

        let mut new_chunks = vec![(3, 1)];
        new_chunks.extend(vec![(7, 1); 1_500]);
        new_chunks.push((4, 1));

        let old = sequence(ChunkSequenceSourceKind::Committed, &old_chunks);
        let new = sequence(ChunkSequenceSourceKind::Committed, &new_chunks);
        assert!(potential_match_pairs(&old.spans, &new.spans) > EXACT_MATCH_PAIR_CEILING);

        let report = compare_sequences("model.bin", &old, &new);
        let metrics = report.chunk_metrics.unwrap();

        assert_eq!(metrics.unchanged_chunks, 1_500);
        assert_eq!(metrics.removed_chunks, 2);
        assert_eq!(metrics.added_chunks, 2);
        assert_eq!(metrics.changed_byte_ranges_old, vec![(0, 1), (1_501, 1)]);
        assert_eq!(metrics.changed_byte_ranges_new, vec![(0, 1), (1_501, 1)]);
    }

    #[test]
    fn staged_sequence_records_source_in_metrics() {
        let old = sequence(ChunkSequenceSourceKind::Committed, &[(1, 10)]);
        let new = ChunkSequence::from_staged(hash(9), 20, &[(hash(1), 10), (hash(2), 10)]);

        let report = compare_sequences("model.bin", &old, &new);
        let metrics = report.chunk_metrics.unwrap();

        assert_eq!(metrics.old_source, ChunkSequenceSourceKind::Committed);
        assert_eq!(metrics.new_source, ChunkSequenceSourceKind::Staged);
        assert_eq!(metrics.added_chunks, 1);
    }
}
