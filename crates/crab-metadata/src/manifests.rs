//! Versioned wrappers with generation counters for CAS-updated mutable state.
//!
//! `PackList` and `ShardList` are the two versioned lists in a crab
//! repository. Each is a JSON object with a monotonically increasing
//! `generation` counter and a list of entry names (pack or shard hashes).
//! Updates go through the JSON CAS loop owned by `crab-storage`.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::segmented::{self, SegmentKind, SegmentWrite, ShardSegmentEntry};
use crate::validation::{corrupt_object, validate_content_hash, validate_sha1};

/// A compacted repository snapshot.
///
/// Stored at `{repo}/manifest`. Contains refs inline and content hashes
/// pointing to immutable segmented indexes. Committed journal transactions
/// newer than this snapshot are materialized on reads until compaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Format version. Always 2 for segmented metadata.
    pub version: u32,
    /// Monotonically increasing generation number.
    pub generation: u64,
    /// ISO 8601 timestamp of the push that created this generation.
    pub created_at: String,
    /// Identity of the pusher.
    pub pusher: Option<String>,
    /// Unique session ID for the push.
    pub session_id: String,
    /// Complete ref map: ref name to SHA.
    pub refs: BTreeMap<String, String>,
    /// Peeled-target map for annotated tags: ref name to target-commit SHA.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peeled_refs: BTreeMap<String, String>,
    /// HEAD symref target, such as `refs/heads/main`.
    pub head: String,
    /// Blake3 hash of the segmented shard-index object.
    pub shard_index_hash: String,
    /// Blake3 hash of the segmented pack-index object.
    pub pack_index_hash: String,
    /// Blake3 commitment to the semantically validated Git state.
    pub git_validation_digest: String,
    /// Blake3 hash of the complete split commit-graph descriptor.
    pub commit_graph_hash: Option<String>,
    /// Blake3 hash of the ref-registry bulk object.
    pub ref_registry_hash: Option<String>,
}

impl Manifest {
    /// Create an empty generation-0 manifest for a freshly initialized repo.
    #[must_use]
    pub fn default_for_repo(head: &str) -> Self {
        let mut manifest = Self {
            version: 2,
            generation: 0,
            created_at: String::new(),
            pusher: None,
            session_id: String::new(),
            refs: BTreeMap::new(),
            peeled_refs: BTreeMap::new(),
            head: head.to_owned(),
            shard_index_hash: String::new(),
            pack_index_hash: String::new(),
            git_validation_digest: String::new(),
            commit_graph_hash: None,
            ref_registry_hash: None,
        };
        manifest.seal_git_validation();
        manifest
    }

    /// Seal the current Git state after its publication owner validates it.
    pub fn seal_git_validation(&mut self) {
        self.git_validation_digest = self.expected_git_validation_digest();
    }

    fn expected_git_validation_digest(&self) -> String {
        fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab manifest validated git state\0");
        hasher.update(&self.generation.to_le_bytes());
        field(&mut hasher, self.pack_index_hash.as_bytes());
        field(&mut hasher, self.head.as_bytes());
        for (name, oid) in &self.refs {
            field(&mut hasher, name.as_bytes());
            field(&mut hasher, oid.as_bytes());
        }
        for (name, oid) in &self.peeled_refs {
            field(&mut hasher, name.as_bytes());
            field(&mut hasher, oid.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Validates a manifest payload before callers trust its references.
///
/// Returns [`MetadataError::CorruptObject`] when the manifest version, ref OIDs,
/// HEAD target, or metadata content hashes are malformed.
pub fn validate_manifest_payload(manifest: &Manifest) -> Result<()> {
    if manifest.version != 2 {
        return Err(corrupt_object("manifest", "manifest must use version 2"));
    }
    for oid in manifest.refs.values() {
        validate_sha1(oid, "manifest ref oid", "manifest")?;
    }
    for oid in manifest.peeled_refs.values() {
        validate_sha1(oid, "manifest peeled ref oid", "manifest")?;
    }
    if !manifest.refs.is_empty() && !manifest.refs.contains_key(&manifest.head) {
        return Err(corrupt_object(
            "manifest",
            "manifest HEAD does not resolve to a ref",
        ));
    }
    validate_optional_content_hash(
        &manifest.shard_index_hash,
        "manifest shard_index_hash",
        "manifest",
    )?;
    validate_optional_content_hash(
        &manifest.pack_index_hash,
        "manifest pack_index_hash",
        "manifest",
    )?;
    validate_content_hash(
        &manifest.git_validation_digest,
        "manifest git_validation_digest",
        "manifest",
    )?;
    if manifest.git_validation_digest != manifest.expected_git_validation_digest() {
        return Err(corrupt_object(
            "manifest",
            "manifest Git validation digest does not match its committed Git state",
        ));
    }
    if let Some(hash) = &manifest.commit_graph_hash {
        validate_content_hash(hash, "manifest commit_graph_hash", "manifest")?;
    }
    if let Some(hash) = &manifest.ref_registry_hash {
        validate_content_hash(hash, "manifest ref_registry_hash", "manifest")?;
    }
    Ok(())
}

fn validate_optional_content_hash(value: &str, field: &str, path: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    validate_content_hash(value, field, path)
}

/// A single entry in the segmented pack index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackManifestEntry {
    /// Pack ID.
    pub pack_id: String,
    /// Pack size in bytes.
    pub size: u64,
    /// Blake3 content hash of the pack.
    pub content_hash: String,
    /// Ref tip commits reachable from this pack.
    pub ref_tips: Vec<String>,
    /// Number of Git objects in this pack.
    pub object_count: u64,
}

/// Validates a segmented pack-index record before callers trust it.
///
/// Returns [`MetadataError::CorruptObject`] when content-addressed IDs, size,
/// object count, or reachable ref-tip fields are malformed.
pub fn validate_pack_manifest_entry(entry: &PackManifestEntry) -> Result<()> {
    validate_content_hash(
        &entry.pack_id,
        "pack metadata pack_id",
        "pack metadata entry",
    )?;
    validate_content_hash(
        &entry.content_hash,
        "pack metadata content_hash",
        "pack metadata entry",
    )?;
    if entry.pack_id != entry.content_hash {
        return Err(corrupt_object(
            "pack metadata entry",
            "pack metadata content_hash must match pack_id",
        ));
    }
    if entry.size == 0 || entry.object_count == 0 {
        return Err(corrupt_object(
            "pack metadata entry",
            "pack metadata entry must describe a non-empty pack",
        ));
    }
    for tip in &entry.ref_tips {
        validate_sha1(tip, "pack metadata ref tip", "pack metadata entry")?;
    }
    Ok(())
}

/// Bulk metadata objects produced during manifest construction.
#[derive(Debug, Clone)]
pub struct BulkData {
    /// New shard segments and index object to upload before manifest CAS.
    pub shard_index: SegmentWrite,
    /// New pack segments and index object to upload before manifest CAS.
    pub pack_index: SegmentWrite,
}

/// Build an append-only shard-index update.
pub fn append_shard_index(
    base: segmented::SegmentIndex,
    generation: u64,
    shard_hashes: &[String],
) -> Result<(String, segmented::SegmentIndex, SegmentWrite)> {
    let entries: Vec<ShardSegmentEntry> = shard_hashes
        .iter()
        .map(|hash| ShardSegmentEntry {
            shard_hash: hash.clone(),
            size: 0,
        })
        .collect();
    segmented::append_records(SegmentKind::Shard, base, generation, &entries)
}

/// Build an append-only pack-index update.
pub fn append_pack_index(
    base: segmented::SegmentIndex,
    generation: u64,
    packs: &[PackManifestEntry],
) -> Result<(String, segmented::SegmentIndex, SegmentWrite)> {
    segmented::append_records(SegmentKind::Pack, base, generation, packs)
}

/// Build a compacted shard-index snapshot.
pub fn compact_shard_index(
    generation: u64,
    shard_hashes: &[String],
) -> Result<(String, segmented::SegmentIndex, SegmentWrite)> {
    let entries: Vec<ShardSegmentEntry> = shard_hashes
        .iter()
        .map(|hash| ShardSegmentEntry {
            shard_hash: hash.clone(),
            size: 0,
        })
        .collect();
    segmented::compact_records(SegmentKind::Shard, generation, &entries)
}

/// Build a compacted pack-index snapshot.
pub fn compact_pack_index(
    generation: u64,
    packs: &[PackManifestEntry],
) -> Result<(String, segmented::SegmentIndex, SegmentWrite)> {
    segmented::compact_records(SegmentKind::Pack, generation, packs)
}

/// Serialize a list of shard hashes into newline-delimited hex format.
#[must_use]
pub fn serialize_shard_list(hashes: &[String]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(hashes.len() * 65);
    for hash in hashes {
        buf.extend_from_slice(hash.as_bytes());
        buf.push(b'\n');
    }
    buf
}

/// Parse a newline-delimited shard list back into hex hash strings.
pub fn parse_shard_list(bytes: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(bytes).map_err(|e| MetadataError::CorruptObject {
        path: "shard-list".to_owned(),
        reason: format!("invalid UTF-8: {e}"),
    })?;

    if text.is_empty() {
        return Ok(Vec::new());
    }

    let mut hashes = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        validate_content_hash(line, "shard-list hash", "shard-list")?;
        hashes.push(line.to_owned());
    }
    Ok(hashes)
}

/// Serialize a list of pack entries into JSONL format.
#[must_use]
pub fn serialize_pack_list(packs: &[PackManifestEntry]) -> Vec<u8> {
    let mut buf = Vec::new();
    for pack in packs {
        if let Ok(json) = serde_json::to_string(pack) {
            buf.extend_from_slice(json.as_bytes());
            buf.push(b'\n');
        }
    }
    buf
}

/// Parse a JSONL-formatted pack list back into pack records.
pub fn parse_pack_list(bytes: &[u8]) -> Result<Vec<PackManifestEntry>> {
    let text = std::str::from_utf8(bytes).map_err(|e| MetadataError::CorruptObject {
        path: "pack-list".to_owned(),
        reason: format!("invalid UTF-8: {e}"),
    })?;

    if text.is_empty() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let entry: PackManifestEntry =
            serde_json::from_str(line).map_err(|e| MetadataError::CorruptObject {
                path: "pack-list".to_owned(),
                reason: format!("line {i}: {e}"),
            })?;
        validate_pack_manifest_entry(&entry)?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Parses and validates pack segment entries from a segment body.
pub fn parse_pack_segment_entries(
    segment: &segmented::SegmentRef,
    bytes: &[u8],
    path: &str,
) -> Result<Vec<PackManifestEntry>> {
    let entries = segmented::parse_segment_records::<PackManifestEntry>(
        SegmentKind::Pack,
        segment,
        bytes,
        path,
    )?;
    validate_pack_manifest_entries(&entries)?;
    Ok(entries)
}

fn validate_pack_manifest_entries(entries: &[PackManifestEntry]) -> Result<()> {
    for entry in entries {
        validate_pack_manifest_entry(entry)?;
    }
    Ok(())
}

/// Set of all commit SHAs reachable from the advertised refs.
#[must_use]
pub fn manifest_reachable_objects(
    manifest: &Manifest,
    graph: Option<&dyn crate::commit_graph::CommitGraphTraversal>,
) -> HashSet<String> {
    let mut reachable: HashSet<String> = manifest.refs.values().cloned().collect();

    let Some(graph) = graph else {
        return reachable;
    };
    let roots = manifest
        .refs
        .iter()
        .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid).clone())
        .collect::<Vec<_>>();
    if let Some(commits) = graph.reachable_to_boundary(&roots, &[]) {
        reachable.extend(commits);
    }

    reachable
}

/// Versioned wrapper with a generation counter for CAS updates.
///
/// The generation counter is bumped on every successful CAS write,
/// providing a total ordering of versions. Readers use the
/// generation to detect stale cached copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versioned<T> {
    /// Monotonically increasing version counter.
    pub generation: u64,
    /// The manifest payload (list of pack names, shard hashes, etc.).
    pub entries: T,
}

impl<T: Default> Default for Versioned<T> {
    fn default() -> Self {
        Self {
            generation: 0,
            entries: T::default(),
        }
    }
}

/// Extended pack entry with optional ref-tip metadata.
///
/// Legacy packs (pushed before ref-based filtering) have `ref_tips: None`
/// and are downloaded unconditionally during fetch. Newer packs carry
/// ref tips so the fetch pipeline can skip irrelevant packs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    /// Pack ID (SHA hash).
    pub pack_id: String,
    /// Pack size in bytes.
    pub size: u64,
    /// Ref tips reachable from this pack (`None` for legacy packs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_tips: Option<Vec<String>>,
}

impl PackEntry {
    /// Create a legacy pack entry without ref-tip metadata.
    pub fn legacy(pack_id: impl Into<String>, size: u64) -> Self {
        Self {
            pack_id: pack_id.into(),
            size,
            ref_tips: None,
        }
    }

    /// Create a pack entry with ref-tip metadata.
    pub fn with_ref_tips(pack_id: impl Into<String>, size: u64, ref_tips: Vec<String>) -> Self {
        Self {
            pack_id: pack_id.into(),
            size,
            ref_tips: Some(ref_tips),
        }
    }
}

/// Mutable list of pack entries, CAS-updated.
pub type PackList = Versioned<Vec<PackEntry>>;

/// Mutable list of shard hashes, CAS-updated.
pub type ShardList = Versioned<Vec<String>>;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn valid_pack_entry() -> PackManifestEntry {
        PackManifestEntry {
            pack_id: "a".repeat(64),
            size: 42,
            content_hash: "a".repeat(64),
            ref_tips: vec!["b".repeat(40)],
            object_count: 3,
        }
    }

    fn segment_ref(kind: SegmentKind, records: u64) -> segmented::SegmentRef {
        let hash = "c".repeat(64);
        segmented::SegmentRef {
            path: segmented::segment_relative_path(kind, &hash),
            hash,
            generation: 1,
            records,
            bytes: 100,
            snapshot: false,
        }
    }

    #[test]
    fn default_pack_list_is_generation_zero() {
        let m: PackList = PackList::default();
        assert_eq!(m.generation, 0);
        assert!(m.entries.is_empty());
    }

    #[test]
    fn shard_list_serialize_deserialize_round_trip() {
        let m = ShardList {
            generation: 42,
            entries: vec!["abc123".to_string(), "def456".to_string()],
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: ShardList = serde_json::from_str(&json).unwrap();
        assert_eq!(m, parsed);
    }

    #[test]
    fn empty_pack_list_serializes_cleanly() {
        let m: PackList = PackList::default();
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"generation\":0"));
        assert!(json.contains("\"entries\":[]"));
    }

    #[test]
    fn pack_entry_legacy_constructor() {
        let entry = PackEntry::legacy("abc123", 1024);
        assert_eq!(entry.pack_id, "abc123");
        assert_eq!(entry.size, 1024);
        assert!(entry.ref_tips.is_none());
    }

    #[test]
    fn pack_entry_with_ref_tips_constructor() {
        let entry = PackEntry::with_ref_tips(
            "def456",
            2048,
            vec!["tip_a".to_string(), "tip_b".to_string()],
        );
        assert_eq!(entry.pack_id, "def456");
        assert_eq!(entry.size, 2048);
        assert_eq!(
            entry.ref_tips,
            Some(vec!["tip_a".to_string(), "tip_b".to_string()])
        );
    }

    #[test]
    fn pack_list_round_trip_with_ref_tips() {
        let m = PackList {
            generation: 5,
            entries: vec![
                PackEntry::with_ref_tips("pack1", 100, vec!["sha_a".to_string()]),
                PackEntry::legacy("pack2", 200),
            ],
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: PackList = serde_json::from_str(&json).unwrap();
        assert_eq!(m, parsed);
    }

    #[test]
    fn legacy_pack_entry_omits_ref_tips_in_json() {
        let entry = PackEntry::legacy("abc", 512);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("ref_tips"));
    }

    #[test]
    fn deserialize_pack_entry_without_ref_tips_field() {
        let json = r#"{"pack_id":"old_pack","size":4096}"#;
        let entry: PackEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.pack_id, "old_pack");
        assert_eq!(entry.size, 4096);
        assert!(entry.ref_tips.is_none());
    }

    #[test]
    fn default_manifest_is_generation_zero() {
        let manifest = Manifest::default_for_repo("refs/heads/main");

        assert_eq!(manifest.version, 2);
        assert_eq!(manifest.generation, 0);
        assert_eq!(manifest.head, "refs/heads/main");
        assert!(manifest.refs.is_empty());
        assert!(manifest.peeled_refs.is_empty());
    }

    #[test]
    fn manifest_ref_serialization_is_deterministic() {
        let mut first = Manifest::default_for_repo("refs/heads/main");
        first.refs.insert("refs/heads/z".into(), "z".repeat(40));
        first.refs.insert("refs/heads/a".into(), "a".repeat(40));

        let json = serde_json::to_string(&first).unwrap();
        let a = json.find("refs/heads/a").unwrap();
        let z = json.find("refs/heads/z").unwrap();

        assert!(a < z);
    }

    #[test]
    fn manifest_payload_validation_accepts_default_manifest() {
        validate_manifest_payload(&Manifest::default_for_repo("refs/heads/main")).unwrap();
    }

    #[test]
    fn manifest_decode_requires_git_validation_digest() {
        let manifest = Manifest::default_for_repo("refs/heads/main");
        let mut json = serde_json::to_value(manifest).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("git_validation_digest");

        assert!(serde_json::from_value::<Manifest>(json).is_err());
    }

    #[test]
    fn git_validation_digest_binds_complete_git_state() {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest
            .refs
            .insert("refs/heads/main".into(), "a".repeat(40));
        manifest
            .peeled_refs
            .insert("refs/tags/v1".into(), "b".repeat(40));
        manifest.pack_index_hash = "c".repeat(64);
        manifest.seal_git_validation();
        validate_manifest_payload(&manifest).unwrap();

        let mut mutations = Vec::new();
        let mut changed = manifest.clone();
        changed.generation += 1;
        mutations.push(changed);
        let mut changed = manifest.clone();
        changed.head = "refs/heads/other".into();
        mutations.push(changed);
        let mut changed = manifest.clone();
        changed.refs.insert("refs/heads/dev".into(), "d".repeat(40));
        mutations.push(changed);
        let mut changed = manifest.clone();
        changed
            .peeled_refs
            .insert("refs/tags/v2".into(), "e".repeat(40));
        mutations.push(changed);
        let mut changed = manifest;
        changed.pack_index_hash = "f".repeat(64);
        mutations.push(changed);

        for mut changed in mutations {
            assert!(validate_manifest_payload(&changed).is_err());
            changed.seal_git_validation();
            if changed.refs.contains_key(&changed.head) {
                validate_manifest_payload(&changed).unwrap();
            }
        }
    }

    #[test]
    fn manifest_payload_validation_rejects_malformed_fields() {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest
            .refs
            .insert("refs/heads/main".into(), "a".repeat(40));
        manifest.shard_index_hash = "b".repeat(64);
        manifest.pack_index_hash = "c".repeat(64);

        let mut bad_version = manifest.clone();
        bad_version.version = 1;
        assert!(validate_manifest_payload(&bad_version).is_err());

        let mut bad_ref = manifest.clone();
        bad_ref
            .refs
            .insert("refs/heads/dev".into(), "not-a-sha".into());
        assert!(validate_manifest_payload(&bad_ref).is_err());

        let mut bad_head = manifest.clone();
        bad_head.head = "refs/heads/missing".into();
        assert!(validate_manifest_payload(&bad_head).is_err());

        let mut bad_index_hash = manifest;
        bad_index_hash.pack_index_hash = "bad-index".into();
        assert!(validate_manifest_payload(&bad_index_hash).is_err());
    }

    #[test]
    fn pack_manifest_entry_round_trips() {
        let entry = PackManifestEntry {
            pack_id: "pack-1".to_owned(),
            size: 42,
            content_hash: "a".repeat(64),
            ref_tips: vec!["b".repeat(40)],
            object_count: 3,
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: PackManifestEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, entry);
    }

    #[test]
    fn pack_manifest_entry_validation_accepts_content_addressed_pack() {
        let entry = valid_pack_entry();

        validate_pack_manifest_entry(&entry).unwrap();
    }

    #[test]
    fn pack_manifest_entry_validation_rejects_malformed_fields() {
        let valid = valid_pack_entry();

        let mut bad_pack_id = valid.clone();
        bad_pack_id.pack_id = "../manifest".to_owned();
        assert!(validate_pack_manifest_entry(&bad_pack_id).is_err());

        let mut mismatched_content_hash = valid.clone();
        mismatched_content_hash.content_hash = "c".repeat(64);
        assert!(validate_pack_manifest_entry(&mismatched_content_hash).is_err());

        let mut empty_pack = valid.clone();
        empty_pack.object_count = 0;
        assert!(validate_pack_manifest_entry(&empty_pack).is_err());

        let mut bad_ref_tip = valid;
        bad_ref_tip.ref_tips = vec!["not-a-sha".to_owned()];
        assert!(validate_pack_manifest_entry(&bad_ref_tip).is_err());
    }

    #[test]
    fn shard_list_parser_rejects_malformed_hashes() {
        assert!(parse_shard_list(b"not-a-hash\n").is_err());
    }

    #[test]
    fn pack_list_parser_validates_entries() {
        let mut invalid = valid_pack_entry();
        invalid.content_hash = "d".repeat(64);
        let bytes = serialize_pack_list(&[invalid]);

        assert!(parse_pack_list(&bytes).is_err());
    }

    #[test]
    fn pack_segment_parser_validates_count_and_entries() {
        let valid = valid_pack_entry();
        let bytes = segmented::to_jsonl(&[valid.clone()]).unwrap();
        let parsed =
            parse_pack_segment_entries(&segment_ref(SegmentKind::Pack, 1), &bytes, "pack").unwrap();
        assert_eq!(parsed, vec![valid]);

        assert!(
            parse_pack_segment_entries(&segment_ref(SegmentKind::Pack, 2), &bytes, "pack").is_err()
        );
    }
}
