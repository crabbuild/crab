//! Segmented append-only pack/shard metadata.
//!
//! A push writes a small immutable segment containing only new metadata
//! records, then writes a small index object that references prior segments
//! plus the new one. Compaction rewrites many segments into one snapshot.

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{corrupt_object, validate_content_hash};

/// Segment index object format version.
pub const SEGMENT_INDEX_VERSION: u32 = 1;

/// Metadata collection kind stored under `{repo}/metadata/{kind}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Pack,
    Shard,
}

impl SegmentKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pack => "pack",
            Self::Shard => "shard",
        }
    }
}

/// Segment index object stored by content hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentIndex {
    /// Format version. Hard-cut v2 metadata starts at 1 for this object.
    pub version: u32,
    /// Monotonic metadata generation.
    pub generation: u64,
    /// Ordered segment references.
    pub segments: Vec<SegmentRef>,
    /// Total logical records across all referenced segments.
    pub total_records: u64,
    /// Total serialized segment bytes.
    pub total_bytes: u64,
}

impl Default for SegmentIndex {
    fn default() -> Self {
        Self {
            version: SEGMENT_INDEX_VERSION,
            generation: 0,
            segments: Vec::new(),
            total_records: 0,
            total_bytes: 0,
        }
    }
}

/// Reference to one immutable metadata segment object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentRef {
    /// Segment content hash.
    pub hash: String,
    /// Segment object path relative to the repo prefix.
    pub path: String,
    /// Segment generation.
    pub generation: u64,
    /// Number of JSONL records in the segment.
    pub records: u64,
    /// Serialized byte length.
    pub bytes: u64,
    /// Whether this segment is a compaction snapshot.
    #[serde(default)]
    pub snapshot: bool,
}

/// Pack metadata segment entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackSegmentEntry {
    pub pack_id: String,
    pub size: u64,
    pub content_hash: String,
    pub ref_tips: Vec<String>,
    pub object_count: u64,
}

/// Shard metadata segment entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardSegmentEntry {
    pub shard_hash: String,
    pub size: u64,
}

/// Immutable segment body plus its reference.
#[derive(Debug, Clone)]
pub struct SegmentObject {
    pub reference: SegmentRef,
    pub bytes: Vec<u8>,
}

/// Index body plus the parsed index.
#[derive(Debug, Clone)]
pub struct SegmentIndexObject {
    pub kind: SegmentKind,
    pub hash: String,
    pub index: SegmentIndex,
    pub bytes: Vec<u8>,
}

/// Objects a push must upload before CASing the manifest pointer.
#[derive(Debug, Clone, Default)]
pub struct SegmentWrite {
    pub segments: Vec<SegmentObject>,
    pub index: Option<SegmentIndexObject>,
}

/// Append a segment ref to an index.
#[must_use]
pub fn append_segment(mut index: SegmentIndex, segment: SegmentRef) -> SegmentIndex {
    index.generation = index.generation.max(segment.generation);
    index.total_records = index.total_records.saturating_add(segment.records);
    index.total_bytes = index.total_bytes.saturating_add(segment.bytes);
    index.segments.push(segment);
    index
}

/// Object path stored in a [`SegmentRef`].
#[must_use]
pub fn segment_relative_path(kind: SegmentKind, hash: &str) -> String {
    format!("metadata/{}/segments/{hash}.jsonl", kind.as_str())
}

/// Object path for a segment index.
#[must_use]
pub fn index_relative_path(kind: SegmentKind, hash: &str) -> String {
    format!("metadata/{}/indexes/{hash}.json", kind.as_str())
}

/// Build a segment object from records.
pub fn build_segment<T: Serialize>(
    kind: SegmentKind,
    generation: u64,
    snapshot: bool,
    records: &[T],
) -> Result<Option<SegmentObject>> {
    if records.is_empty() {
        return Ok(None);
    }

    let bytes = to_jsonl(records)?;
    let hash = content_hash(&bytes);
    let reference = SegmentRef {
        path: segment_relative_path(kind, &hash),
        hash,
        generation,
        records: records.len() as u64,
        bytes: bytes.len() as u64,
        snapshot,
    };
    Ok(Some(SegmentObject { reference, bytes }))
}

/// Build an immutable index object.
pub fn build_index_object(kind: SegmentKind, index: SegmentIndex) -> Result<SegmentIndexObject> {
    let bytes = serde_json::to_vec_pretty(&index)
        .map_err(|e| MetadataError::Internal(format!("segment index serialize: {e}")))?;
    let hash = content_hash(&bytes);
    Ok(SegmentIndexObject {
        kind,
        hash,
        index,
        bytes,
    })
}

/// Parses a segment index object from JSON bytes.
///
/// Returns [`MetadataError::CorruptObject`] with `path` when the payload is not
/// valid JSON for [`SegmentIndex`].
pub fn parse_segment_index(bytes: &[u8], path: &str) -> Result<SegmentIndex> {
    serde_json::from_slice(bytes)
        .map_err(|error| corrupt_object(path, format!("invalid segment index JSON: {error}")))
}

/// Validates a segmented metadata index before callers trust its references.
///
/// Returns [`MetadataError::CorruptObject`] when version, segment paths, segment
/// sizes, or aggregate totals are malformed.
pub fn validate_segment_index_shape(kind: SegmentKind, index: &SegmentIndex) -> Result<()> {
    let path = format!("{} metadata index", kind.as_str());
    if index.version != SEGMENT_INDEX_VERSION {
        return Err(corrupt_object(
            &path,
            format!("unsupported segment index version {}", index.version),
        ));
    }

    let mut total_records = 0u64;
    let mut total_bytes = 0u64;
    for segment in &index.segments {
        validate_content_hash(&segment.hash, "metadata segment hash", &path)?;
        let expected_path = segment_relative_path(kind, &segment.hash);
        if segment.path != expected_path {
            return Err(corrupt_object(
                &path,
                format!(
                    "{} metadata segment path does not match its hash",
                    kind.as_str()
                ),
            ));
        }
        if segment.records == 0 || segment.bytes == 0 {
            return Err(corrupt_object(
                &path,
                format!("{} metadata segment is empty", kind.as_str()),
            ));
        }
        total_records = total_records.saturating_add(segment.records);
        total_bytes = total_bytes.saturating_add(segment.bytes);
    }

    if index.total_records != total_records || index.total_bytes != total_bytes {
        return Err(corrupt_object(
            &path,
            format!(
                "{} metadata index totals do not match segments",
                kind.as_str()
            ),
        ));
    }
    Ok(())
}

/// Validates that a candidate segmented index only appends to a base index.
///
/// Returns [`MetadataError::CorruptObject`] when the candidate drops, rewrites,
/// or regresses existing segment references.
pub fn validate_append_only_index(
    kind: SegmentKind,
    base: &SegmentIndex,
    candidate: &SegmentIndex,
) -> Result<()> {
    let path = format!("{} metadata index", kind.as_str());
    if candidate.segments.len() < base.segments.len() {
        return Err(corrupt_object(
            &path,
            format!("{} metadata index dropped base segments", kind.as_str()),
        ));
    }
    if candidate.segments[..base.segments.len()] != base.segments {
        return Err(corrupt_object(
            &path,
            format!("{} metadata index rewrote base segments", kind.as_str()),
        ));
    }
    if base.segments.is_empty() && candidate.segments.is_empty() {
        return Ok(());
    }
    if candidate.segments.len() == base.segments.len() {
        return Err(corrupt_object(
            &path,
            format!(
                "{} metadata index changed without appended segments",
                kind.as_str()
            ),
        ));
    }
    if candidate.generation < base.generation {
        return Err(corrupt_object(
            &path,
            format!(
                "{} metadata index generation moved backwards",
                kind.as_str()
            ),
        ));
    }
    Ok(())
}

/// Parses a segment body and verifies it matches the segment reference.
///
/// Returns [`MetadataError::CorruptObject`] when the JSONL body is malformed or
/// contains a different record count than `segment.records`.
pub fn parse_segment_records<T: for<'de> Deserialize<'de>>(
    kind: SegmentKind,
    segment: &SegmentRef,
    bytes: &[u8],
    path: &str,
) -> Result<Vec<T>> {
    let records = from_jsonl::<T>(bytes, path)?;
    validate_segment_record_count(kind, segment, records.len())?;
    Ok(records)
}

/// Validates a shard segment entry before callers trust its shard reference.
///
/// Shard segment `size` is currently advisory and may be zero for legacy
/// writers, so the hash is the canonical trust-bearing field.
pub fn validate_shard_segment_entry(entry: &ShardSegmentEntry) -> Result<()> {
    validate_content_hash(
        &entry.shard_hash,
        "shard segment entry hash",
        "shard segment entry",
    )
}

/// Parses and validates shard segment entries from a segment body.
pub fn parse_shard_segment_entries(
    segment: &SegmentRef,
    bytes: &[u8],
    path: &str,
) -> Result<Vec<ShardSegmentEntry>> {
    let entries =
        parse_segment_records::<ShardSegmentEntry>(SegmentKind::Shard, segment, bytes, path)?;
    for entry in &entries {
        validate_shard_segment_entry(entry)?;
    }
    Ok(entries)
}

fn validate_segment_record_count(
    kind: SegmentKind,
    segment: &SegmentRef,
    actual_records: usize,
) -> Result<()> {
    if actual_records as u64 != segment.records {
        return Err(corrupt_object(
            &segment.path,
            format!("{} metadata segment record count mismatch", kind.as_str()),
        ));
    }
    Ok(())
}

/// Append records to an existing index and return objects that need upload.
pub fn append_records<T: Serialize>(
    kind: SegmentKind,
    base: SegmentIndex,
    generation: u64,
    records: &[T],
) -> Result<(String, SegmentIndex, SegmentWrite)> {
    if records.is_empty() {
        let index_object = build_index_object(kind, base.clone())?;
        let hash = index_object.hash.clone();
        return Ok((
            hash,
            index_object.index.clone(),
            SegmentWrite {
                segments: Vec::new(),
                index: Some(index_object),
            },
        ));
    }

    let segment = build_segment(kind, generation, false, records)?.ok_or_else(|| {
        MetadataError::Internal(
            "segment builder returned no segment for non-empty records".to_owned(),
        )
    })?;
    let next = append_segment(base, segment.reference.clone());
    let index_object = build_index_object(kind, next.clone())?;
    let hash = index_object.hash.clone();
    Ok((
        hash,
        next,
        SegmentWrite {
            segments: vec![segment],
            index: Some(index_object),
        },
    ))
}

/// Build a compacted snapshot index from records.
pub fn compact_records<T: Serialize>(
    kind: SegmentKind,
    generation: u64,
    records: &[T],
) -> Result<(String, SegmentIndex, SegmentWrite)> {
    match build_segment(kind, generation, true, records)? {
        Some(segment) => {
            let index = compact_to_snapshot(generation, segment.reference.clone());
            let index_object = build_index_object(kind, index.clone())?;
            let hash = index_object.hash.clone();
            Ok((
                hash,
                index,
                SegmentWrite {
                    segments: vec![segment],
                    index: Some(index_object),
                },
            ))
        }
        None => {
            let index = SegmentIndex {
                generation,
                ..SegmentIndex::default()
            };
            let index_object = build_index_object(kind, index.clone())?;
            let hash = index_object.hash.clone();
            Ok((
                hash,
                index,
                SegmentWrite {
                    segments: Vec::new(),
                    index: Some(index_object),
                },
            ))
        }
    }
}

/// Replace an index with one compacted snapshot segment.
#[must_use]
pub fn compact_to_snapshot(generation: u64, segment: SegmentRef) -> SegmentIndex {
    SegmentIndex {
        version: SEGMENT_INDEX_VERSION,
        generation,
        total_records: segment.records,
        total_bytes: segment.bytes,
        segments: vec![SegmentRef {
            snapshot: true,
            ..segment
        }],
    }
}

/// Serialize records as JSONL.
pub fn to_jsonl<T: Serialize>(records: &[T]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for record in records {
        serde_json::to_writer(&mut out, record)
            .map_err(|e| MetadataError::Internal(format!("segment serialize: {e}")))?;
        out.push(b'\n');
    }
    Ok(out)
}

/// Parse JSONL records.
pub fn from_jsonl<T: for<'de> Deserialize<'de>>(bytes: &[u8], label: &str) -> Result<Vec<T>> {
    let text = std::str::from_utf8(bytes).map_err(|e| MetadataError::CorruptObject {
        path: label.to_owned(),
        reason: format!("invalid UTF-8: {e}"),
    })?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(line).map_err(|e| MetadataError::CorruptObject {
                path: label.to_owned(),
                reason: format!("line {idx}: {e}"),
            })?,
        );
    }
    Ok(out)
}

/// Content hash for a segment or index body.
#[must_use]
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_hash(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn segment_ref(kind: SegmentKind, ch: char) -> SegmentRef {
        let hash = valid_hash(ch);
        SegmentRef {
            path: segment_relative_path(kind, &hash),
            hash,
            generation: 4,
            records: 2,
            bytes: 99,
            snapshot: false,
        }
    }

    #[test]
    fn pack_segment_round_trips_jsonl() {
        let entries = vec![PackSegmentEntry {
            pack_id: "pack-a".to_owned(),
            size: 42,
            content_hash: "abc".to_owned(),
            ref_tips: vec!["tip".to_owned()],
            object_count: 7,
        }];
        let bytes = to_jsonl(&entries).unwrap();
        let parsed: Vec<PackSegmentEntry> = from_jsonl(&bytes, "pack-segment").unwrap();
        assert_eq!(parsed, entries);
    }

    #[test]
    fn append_updates_generation_counts_and_bytes() {
        let segment = SegmentRef {
            generation: 3,
            ..segment_ref(SegmentKind::Pack, 'a')
        };
        let index = append_segment(SegmentIndex::default(), segment);
        assert_eq!(index.generation, 3);
        assert_eq!(index.total_records, 2);
        assert_eq!(index.total_bytes, 99);
        assert_eq!(index.segments.len(), 1);
    }

    #[test]
    fn compact_snapshot_replaces_prior_segments() {
        let segment = SegmentRef {
            generation: 8,
            records: 10,
            bytes: 500,
            ..segment_ref(SegmentKind::Shard, 'b')
        };
        let index = compact_to_snapshot(8, segment);
        assert_eq!(index.segments.len(), 1);
        assert!(index.segments[0].snapshot);
        assert_eq!(index.total_records, 10);
    }

    #[test]
    fn parse_segment_index_reports_malformed_json_as_corrupt_object() {
        let error = parse_segment_index(b"{", "metadata/pack/indexes/bad.json").unwrap_err();

        assert!(error.to_string().contains("invalid segment index JSON"));
    }

    #[test]
    fn segment_index_validation_accepts_matching_totals() {
        let index = append_segment(SegmentIndex::default(), segment_ref(SegmentKind::Pack, 'c'));

        validate_segment_index_shape(SegmentKind::Pack, &index).unwrap();
    }

    #[test]
    fn segment_index_validation_rejects_malformed_shape() {
        let mut index = append_segment(
            SegmentIndex::default(),
            segment_ref(SegmentKind::Shard, 'd'),
        );
        index.segments[0].path = "metadata/shard/segments/other.jsonl".to_owned();
        assert!(validate_segment_index_shape(SegmentKind::Shard, &index).is_err());

        let mut bad_total =
            append_segment(SegmentIndex::default(), segment_ref(SegmentKind::Pack, 'e'));
        bad_total.total_records = 10;
        assert!(validate_segment_index_shape(SegmentKind::Pack, &bad_total).is_err());
    }

    #[test]
    fn append_only_index_validation_rejects_rewritten_base() {
        let base = append_segment(SegmentIndex::default(), segment_ref(SegmentKind::Pack, 'f'));
        let candidate =
            append_segment(SegmentIndex::default(), segment_ref(SegmentKind::Pack, '1'));

        assert!(validate_append_only_index(SegmentKind::Pack, &base, &candidate).is_err());
    }

    #[test]
    fn parse_segment_records_rejects_record_count_mismatch() {
        let segment = SegmentRef {
            records: 2,
            ..segment_ref(SegmentKind::Shard, 'a')
        };
        let bytes = to_jsonl(&[ShardSegmentEntry {
            shard_hash: valid_hash('b'),
            size: 0,
        }])
        .unwrap();

        assert!(
            parse_segment_records::<ShardSegmentEntry>(
                SegmentKind::Shard,
                &segment,
                &bytes,
                "metadata/shard/segments/bad.jsonl",
            )
            .is_err()
        );
    }

    #[test]
    fn shard_segment_entry_validation_rejects_malformed_hash() {
        let segment = SegmentRef {
            records: 1,
            ..segment_ref(SegmentKind::Shard, 'c')
        };
        let bytes = to_jsonl(&[ShardSegmentEntry {
            shard_hash: "not-a-hash".to_owned(),
            size: 0,
        }])
        .unwrap();

        assert!(parse_shard_segment_entries(&segment, &bytes, "shard-segment").is_err());
    }
}
