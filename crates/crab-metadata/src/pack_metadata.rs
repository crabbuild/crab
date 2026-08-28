//! Per-pack annotation for ref-based fetch filtering.
//!
//! [`PackMetadata`] records ref tip commits known to be reachable from objects
//! in a pack. Stored alongside the pack at `packs/pack-{sha}.meta` as JSON.
//! A content-addressed pack can be selected by more than one push, so writers
//! CAS-union tips while the committed pack-index entry may contain a subset.

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::manifests::PackManifestEntry;
use crate::validation::{corrupt_object, validate_content_hash, validate_sha1};

/// Maximum size of a per-pack ref-tip sidecar read into memory.
///
/// The sidecar is an optional fetch-filtering hint. Readers that encounter a
/// larger object can safely treat it as legacy metadata, while writers replace
/// an oversized ref-tip list with an empty hint before publishing.
pub const MAX_PACK_METADATA_BYTES: u64 = 8 * 1024 * 1024;

/// Per-pack annotation recording which ref tip commits are reachable
/// from objects in the pack.
///
/// Enables ref-based fetch filtering: the fetch pipeline can skip packs
/// whose `ref_tips` do not intersect the requested refs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackMetadata {
    /// Pack ID (SHA hash).
    pub pack_id: String,
    /// Ref tip commit OIDs reachable from this pack's objects.
    pub ref_tips: Vec<String>,
    /// Object count in the pack.
    pub object_count: u64,
}

/// Serializes a pack metadata sidecar when it fits the bounded sidecar format.
///
/// Returns `Ok(None)` when the ref-tip hint would exceed
/// [`MAX_PACK_METADATA_BYTES`]. Callers can safely publish an empty hint in
/// that case; fetch then retains the pack instead of relying on incomplete
/// filtering metadata.
pub fn serialize_pack_metadata_bounded(metadata: &PackMetadata) -> Result<Option<Vec<u8>>> {
    let bytes = serde_json::to_vec(metadata).map_err(|error| {
        MetadataError::Internal(format!("pack metadata serialization failed: {error}"))
    })?;
    if u64::try_from(bytes.len()).is_ok_and(|size| size > MAX_PACK_METADATA_BYTES) {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Parses a pack metadata sidecar from JSON bytes.
///
/// Returns [`crate::error::MetadataError::CorruptObject`] with `path` when the
/// payload is not valid JSON for [`PackMetadata`].
pub fn parse_pack_metadata(bytes: &[u8], path: &str) -> Result<PackMetadata> {
    if u64::try_from(bytes.len()).is_ok_and(|size| size > MAX_PACK_METADATA_BYTES) {
        return Err(corrupt_object(
            path,
            format!(
                "pack metadata is {} bytes; bounded reads support at most {MAX_PACK_METADATA_BYTES} bytes",
                bytes.len()
            ),
        ));
    }
    serde_json::from_slice(bytes)
        .map_err(|error| corrupt_object(path, format!("invalid pack metadata JSON: {error}")))
}

/// Validates that a pack metadata sidecar matches its pack-index entry.
///
/// Returns [`crate::error::MetadataError::CorruptObject`] when the sidecar's
/// pack ID or object count disagrees with `entry`, or the sidecar does not
/// cover every reachable ref tip committed by `entry`.
pub fn validate_pack_metadata_for_entry(
    metadata: &PackMetadata,
    entry: &PackManifestEntry,
) -> Result<()> {
    validate_content_hash(&metadata.pack_id, "pack metadata pack_id", "pack metadata")?;
    if metadata.pack_id != entry.pack_id {
        return Err(corrupt_object(
            "pack metadata",
            "pack metadata pack_id does not match pack entry",
        ));
    }
    if metadata.object_count != entry.object_count {
        return Err(corrupt_object(
            "pack metadata",
            "pack metadata object_count does not match pack entry",
        ));
    }
    for tip in &metadata.ref_tips {
        validate_sha1(tip, "pack metadata ref tip", "pack metadata")?;
    }
    let metadata_tips = metadata
        .ref_tips
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    if entry
        .ref_tips
        .iter()
        .any(|tip| !metadata_tips.contains(tip.as_str()))
    {
        return Err(corrupt_object(
            "pack metadata",
            "pack metadata does not cover pack entry ref_tips",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn serialize_deserialize_round_trip() {
        let meta = PackMetadata {
            pack_id: "abc123def456".to_string(),
            ref_tips: vec![
                "refs/heads/main:sha1".to_string(),
                "refs/heads/dev:sha2".to_string(),
            ],
            object_count: 1234,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: PackMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.pack_id, "abc123def456");
        assert_eq!(parsed.ref_tips.len(), 2);
        assert_eq!(parsed.ref_tips[0], "refs/heads/main:sha1");
        assert_eq!(parsed.ref_tips[1], "refs/heads/dev:sha2");
        assert_eq!(parsed.object_count, 1234);
    }

    #[test]
    fn empty_ref_tips_round_trip() {
        let meta = PackMetadata {
            pack_id: "empty_pack".to_string(),
            ref_tips: vec![],
            object_count: 0,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: PackMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.pack_id, "empty_pack");
        assert!(parsed.ref_tips.is_empty());
        assert_eq!(parsed.object_count, 0);
    }

    #[test]
    fn clone_preserves_all_fields() {
        let meta = PackMetadata {
            pack_id: "pack42".to_string(),
            ref_tips: vec!["tip_a".to_string(), "tip_b".to_string()],
            object_count: 999,
        };
        let cloned = meta.clone();

        assert_eq!(cloned.pack_id, meta.pack_id);
        assert_eq!(cloned.ref_tips, meta.ref_tips);
        assert_eq!(cloned.object_count, meta.object_count);
    }

    #[test]
    fn debug_format_is_readable() {
        let meta = PackMetadata {
            pack_id: "dbg01".to_string(),
            ref_tips: vec!["tip1".to_string()],
            object_count: 7,
        };
        let debug = format!("{meta:?}");

        assert!(debug.contains("dbg01"));
        assert!(debug.contains("tip1"));
        assert!(debug.contains("7"));
    }

    fn valid_entry() -> PackManifestEntry {
        PackManifestEntry {
            pack_id: "a".repeat(64),
            size: 42,
            content_hash: "a".repeat(64),
            ref_tips: vec!["b".repeat(40), "c".repeat(40)],
            object_count: 2,
        }
    }

    fn valid_metadata() -> PackMetadata {
        PackMetadata {
            pack_id: "a".repeat(64),
            ref_tips: vec!["c".repeat(40), "b".repeat(40)],
            object_count: 2,
        }
    }

    #[test]
    fn parse_pack_metadata_reports_malformed_json_as_corrupt_object() {
        let error = parse_pack_metadata(b"{", "packs/pack-a.meta").unwrap_err();

        assert!(error.to_string().contains("invalid pack metadata JSON"));
    }

    #[test]
    fn parse_pack_metadata_rejects_oversized_sidecars_before_json_decode() {
        let bytes = vec![b' '; usize::try_from(MAX_PACK_METADATA_BYTES + 1).unwrap()];

        let error = parse_pack_metadata(&bytes, "packs/pack-a.meta").unwrap_err();

        assert!(error.to_string().contains("bounded reads support"));
    }

    #[test]
    fn serialize_pack_metadata_returns_none_when_ref_tip_hint_is_oversized() {
        let metadata = PackMetadata {
            pack_id: "a".repeat(64),
            ref_tips: (0..200_000).map(|tip| format!("{tip:040x}")).collect(),
            object_count: 200_000,
        };

        assert!(
            serialize_pack_metadata_bounded(&metadata)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pack_metadata_validation_accepts_matching_entry() {
        validate_pack_metadata_for_entry(&valid_metadata(), &valid_entry()).unwrap();
    }

    #[test]
    fn pack_metadata_validation_accepts_cas_union_superset() {
        let mut metadata = valid_metadata();
        metadata.ref_tips.push("d".repeat(40));

        validate_pack_metadata_for_entry(&metadata, &valid_entry()).unwrap();
    }

    #[test]
    fn pack_metadata_validation_rejects_mismatched_fields() {
        let entry = valid_entry();

        let mut bad_pack_id = valid_metadata();
        bad_pack_id.pack_id = "d".repeat(64);
        assert!(validate_pack_metadata_for_entry(&bad_pack_id, &entry).is_err());

        let mut bad_object_count = valid_metadata();
        bad_object_count.object_count = 1;
        assert!(validate_pack_metadata_for_entry(&bad_object_count, &entry).is_err());

        let mut bad_tip = valid_metadata();
        bad_tip.ref_tips = vec!["not-a-sha".to_owned()];
        assert!(validate_pack_metadata_for_entry(&bad_tip, &entry).is_err());

        let mut missing_tip = valid_metadata();
        missing_tip.ref_tips = vec!["b".repeat(40)];
        assert!(validate_pack_metadata_for_entry(&missing_tip, &entry).is_err());
    }
}
