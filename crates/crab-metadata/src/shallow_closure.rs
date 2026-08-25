//! Generation-bound object closures for common shallow-fetch depths.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

const ENTRY_MAGIC: &[u8; 8] = b"CRABSC01";
const ENTRY_VERSION: u32 = 1;
const ENTRY_HEADER_BYTES: usize = 44;
const MAX_ENTRIES: usize = 4_096;
const MAX_OBJECTS_PER_ENTRY: usize = 10_000_000;

/// Depth profiles maintained by the generation owner for initial clones.
pub const DEFAULT_SHALLOW_CLOSURE_DEPTHS: &[u32] = &[1, 10, 100, 1_000];

/// Maximum bytes accepted for one shallow-closure entry.
pub const DEFAULT_MAX_SHALLOW_CLOSURE_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum bytes accepted for one shallow-closure descriptor.
pub const DEFAULT_MAX_SHALLOW_CLOSURE_DESCRIPTOR_BYTES: u64 = 4 * 1024 * 1024;

/// Descriptor for immutable, depth-specific shallow object closures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShallowClosureDescriptor {
    /// Serialized format version.
    pub version: u32,
    /// Manifest generation described by every entry.
    pub generation: u64,
    /// Pack inventory identity described by every entry.
    pub pack_index_hash: String,
    /// Git-state digest used as the descriptor path and binding.
    pub git_validation_digest: String,
    /// Immutable depth-specific entry references.
    pub entries: Vec<ShallowClosureEntryRef>,
}

/// Content-addressed reference to one shallow closure entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShallowClosureEntryRef {
    /// Commit tip for which this closure was computed.
    pub tip: String,
    /// Requested initial clone depth.
    pub depth: u32,
    /// Content hash of the binary entry.
    pub hash: String,
    /// Repository-relative entry path.
    pub path: String,
    /// Exact encoded byte length.
    pub bytes: u64,
    /// Number of selected Git objects.
    pub object_count: u32,
    /// Number of shallow boundary commits.
    pub boundary_count: u32,
}

/// Exact object IDs and shallow boundaries for one depth profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShallowClosureEntry {
    /// Commit tip for which this closure was computed.
    pub tip: [u8; 20],
    /// Requested initial clone depth.
    pub depth: u32,
    /// Exact object IDs in the shallow clone.
    pub object_ids: Vec<[u8; 20]>,
    /// Commits that Git marks shallow for this depth.
    pub shallow: Vec<[u8; 20]>,
}

/// Immutable objects that must be uploaded before the descriptor path.
#[derive(Debug, Clone)]
pub struct ShallowClosureWrite {
    /// Encoded descriptor payload.
    pub descriptor_bytes: Vec<u8>,
    /// Encoded content-addressed entry payloads.
    pub entries: Vec<ShallowClosureEntryObject>,
}

/// One encoded shallow-closure entry and its content-addressed reference.
#[derive(Debug, Clone)]
pub struct ShallowClosureEntryObject {
    /// Descriptor reference for the entry.
    pub reference: ShallowClosureEntryRef,
    /// Encoded entry bytes.
    pub bytes: Vec<u8>,
}

impl ShallowClosureDescriptor {
    /// Validate the descriptor independently of its entry bodies.
    pub fn validate(&self) -> Result<()> {
        if self.version != ENTRY_VERSION {
            return Err(corrupt("unsupported shallow closure descriptor version"));
        }
        validate_content_hash(
            &self.pack_index_hash,
            "shallow closure pack index hash",
            "shallow closure descriptor",
        )?;
        validate_content_hash(
            &self.git_validation_digest,
            "shallow closure Git validation digest",
            "shallow closure descriptor",
        )?;
        if self.entries.is_empty() || self.entries.len() > MAX_ENTRIES {
            return Err(corrupt("shallow closure descriptor entry count is invalid"));
        }
        let mut keys = BTreeSet::new();
        for reference in &self.entries {
            validate_entry_reference(reference)?;
            if !keys.insert((reference.tip.clone(), reference.depth)) {
                return Err(corrupt(
                    "shallow closure descriptor contains duplicate entries",
                ));
            }
        }
        Ok(())
    }

    /// Find the exact entry for one commit tip and depth.
    #[must_use]
    pub fn entry(&self, tip: &[u8; 20], depth: u32) -> Option<&ShallowClosureEntryRef> {
        let tip = sha1_hex(tip);
        self.entries
            .iter()
            .find(|entry| entry.depth == depth && entry.tip == tip)
    }
}

/// Build normalized immutable entries and their descriptor.
pub fn build_shallow_closure_write(
    generation: u64,
    pack_index_hash: String,
    git_validation_digest: String,
    mut entries: Vec<ShallowClosureEntry>,
) -> Result<ShallowClosureWrite> {
    validate_content_hash(
        &pack_index_hash,
        "shallow closure pack index hash",
        "shallow closure",
    )?;
    validate_content_hash(
        &git_validation_digest,
        "shallow closure Git validation digest",
        "shallow closure",
    )?;
    if entries.is_empty() || entries.len() > MAX_ENTRIES {
        return Err(corrupt("shallow closure entry count is invalid"));
    }
    entries.sort_unstable_by_key(|entry| (entry.tip, entry.depth));
    let mut references = Vec::with_capacity(entries.len());
    let mut objects = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut normalized = entry;
        normalized.object_ids.sort_unstable();
        normalized.object_ids.dedup();
        normalized.shallow.sort_unstable();
        normalized.shallow.dedup();
        let bytes = encode_shallow_closure_entry(&normalized)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let reference = ShallowClosureEntryRef {
            tip: sha1_hex(&normalized.tip),
            depth: normalized.depth,
            path: shallow_closure_entry_relative_path(&hash),
            bytes: bytes.len() as u64,
            object_count: u32::try_from(normalized.object_ids.len())
                .map_err(|_| corrupt("shallow closure object count exceeds u32"))?,
            boundary_count: u32::try_from(normalized.shallow.len())
                .map_err(|_| corrupt("shallow closure boundary count exceeds u32"))?,
            hash,
        };
        references.push(reference.clone());
        objects.push(ShallowClosureEntryObject { reference, bytes });
    }
    let descriptor = ShallowClosureDescriptor {
        version: ENTRY_VERSION,
        generation,
        pack_index_hash,
        git_validation_digest,
        entries: references,
    };
    descriptor.validate()?;
    let descriptor_bytes = serde_json::to_vec(&descriptor).map_err(|source| {
        MetadataError::Internal(format!("shallow closure descriptor encode: {source}"))
    })?;
    Ok(ShallowClosureWrite {
        descriptor_bytes,
        entries: objects,
    })
}

/// Rebind an existing closure to a new manifest identity without rebuilding its entries.
pub fn rebind_shallow_closure_write(
    descriptor: &ShallowClosureDescriptor,
    generation: u64,
    pack_index_hash: String,
    git_validation_digest: String,
) -> Result<ShallowClosureWrite> {
    let rebound = ShallowClosureDescriptor {
        version: descriptor.version,
        generation,
        pack_index_hash,
        git_validation_digest,
        entries: descriptor.entries.clone(),
    };
    rebound.validate()?;
    let descriptor_bytes = serde_json::to_vec(&rebound).map_err(|source| {
        MetadataError::Internal(format!("shallow closure descriptor encode: {source}"))
    })?;
    Ok(ShallowClosureWrite {
        descriptor_bytes,
        entries: Vec::new(),
    })
}

/// Decode and validate one binary shallow closure entry.
pub fn decode_shallow_closure_entry(bytes: &[u8], path: &str) -> Result<ShallowClosureEntry> {
    if bytes.len() < ENTRY_HEADER_BYTES || &bytes[..8] != ENTRY_MAGIC {
        return Err(corrupt_at(path, "invalid shallow closure entry header"));
    }
    let mut cursor = 8;
    let version = read_u32(bytes, &mut cursor, path)?;
    if version != ENTRY_VERSION {
        return Err(corrupt_at(
            path,
            "unsupported shallow closure entry version",
        ));
    }
    let tip = read_oid(bytes, &mut cursor, path)?;
    let depth = read_u32(bytes, &mut cursor, path)?;
    if depth == 0 {
        return Err(corrupt_at(path, "shallow closure depth must be positive"));
    }
    let object_count = read_u32(bytes, &mut cursor, path)? as usize;
    let boundary_count = read_u32(bytes, &mut cursor, path)? as usize;
    if object_count == 0 || object_count > MAX_OBJECTS_PER_ENTRY {
        return Err(corrupt_at(path, "shallow closure object count is invalid"));
    }
    let total_ids = object_count
        .checked_add(boundary_count)
        .ok_or_else(|| corrupt_at(path, "shallow closure ID count overflows"))?;
    let expected = ENTRY_HEADER_BYTES
        .checked_add(
            total_ids
                .checked_mul(20)
                .ok_or_else(|| corrupt_at(path, "shallow closure entry size overflows"))?,
        )
        .ok_or_else(|| corrupt_at(path, "shallow closure entry size overflows"))?;
    if expected != bytes.len() {
        return Err(corrupt_at(
            path,
            "shallow closure entry length does not match its header",
        ));
    }
    let mut object_ids = Vec::with_capacity(object_count);
    for _ in 0..object_count {
        object_ids.push(read_oid(bytes, &mut cursor, path)?);
    }
    let mut shallow = Vec::with_capacity(boundary_count);
    for _ in 0..boundary_count {
        shallow.push(read_oid(bytes, &mut cursor, path)?);
    }
    let entry = ShallowClosureEntry {
        tip,
        depth,
        object_ids,
        shallow,
    };
    validate_entry(&entry, path)?;
    Ok(entry)
}

/// Decode and validate one generation-bound descriptor.
pub fn decode_shallow_closure_descriptor(
    bytes: &[u8],
    path: &str,
) -> Result<ShallowClosureDescriptor> {
    let descriptor: ShallowClosureDescriptor =
        serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: format!("invalid shallow closure descriptor: {source}"),
        })?;
    descriptor.validate()?;
    Ok(descriptor)
}

/// Repository-relative path for one immutable shallow closure entry.
#[must_use]
pub fn shallow_closure_entry_relative_path(hash: &str) -> String {
    format!("metadata/shallow-closure/entries/{hash}.bin")
}

#[cfg(feature = "storage")]
/// Load and validate the descriptor bound to one manifest digest.
pub async fn load_shallow_closure_descriptor(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    git_validation_digest: &str,
    expected_generation: u64,
    expected_pack_index_hash: &str,
    max_bytes: u64,
) -> Result<Option<ShallowClosureDescriptor>> {
    validate_content_hash(
        git_validation_digest,
        "shallow closure Git validation digest",
        "shallow closure",
    )?;
    let path = router.shallow_closure_path(git_validation_digest);
    let bytes = match store.get_with_etag(&path).await {
        Ok((bytes, _etag)) => bytes,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(MetadataError::from(error)),
    };
    if bytes.len() as u64 > max_bytes {
        return Err(MetadataError::CorruptObject {
            path: path.to_string(),
            reason: format!("shallow closure descriptor exceeds {max_bytes} bytes"),
        });
    }
    let descriptor = decode_shallow_closure_descriptor(&bytes, path.as_ref())?;
    if descriptor.generation != expected_generation
        || descriptor.pack_index_hash != expected_pack_index_hash
        || descriptor.git_validation_digest != git_validation_digest
    {
        return Err(MetadataError::CorruptObject {
            path: path.to_string(),
            reason: "shallow closure descriptor does not match its manifest".to_owned(),
        });
    }
    Ok(Some(descriptor))
}

#[cfg(feature = "storage")]
/// Load, hash-verify, and decode one immutable shallow closure entry.
pub async fn load_shallow_closure_entry(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    reference: &ShallowClosureEntryRef,
    max_bytes: u64,
) -> Result<ShallowClosureEntry> {
    validate_entry_reference(reference)?;
    if reference.bytes > max_bytes {
        return Err(MetadataError::CorruptObject {
            path: reference.path.clone(),
            reason: format!("shallow closure entry exceeds {max_bytes} bytes"),
        });
    }
    let path = router.repo_path(&reference.path);
    let expected = decode_hash(&reference.hash, path.as_ref())?;
    let bytes = store.verify(&path, &expected).await?;
    if bytes.len() as u64 != reference.bytes {
        return Err(MetadataError::CorruptObject {
            path: path.to_string(),
            reason: "shallow closure entry length mismatch".to_owned(),
        });
    }
    let entry = decode_shallow_closure_entry(&bytes, path.as_ref())?;
    if sha1_hex(&entry.tip) != reference.tip
        || entry.depth != reference.depth
        || entry.object_ids.len() != reference.object_count as usize
        || entry.shallow.len() != reference.boundary_count as usize
    {
        return Err(MetadataError::CorruptObject {
            path: path.to_string(),
            reason: "shallow closure entry does not match its descriptor".to_owned(),
        });
    }
    Ok(entry)
}

#[cfg(feature = "storage")]
/// Upload immutable entry bodies before the manifest-bound descriptor.
pub async fn upload_shallow_closure(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    git_validation_digest: &str,
    write: &ShallowClosureWrite,
) -> Result<()> {
    validate_content_hash(
        git_validation_digest,
        "shallow closure Git validation digest",
        "shallow closure",
    )?;
    let descriptor_path = router.shallow_closure_path(git_validation_digest);
    let descriptor =
        decode_shallow_closure_descriptor(&write.descriptor_bytes, descriptor_path.as_ref())?;
    if descriptor.git_validation_digest != git_validation_digest {
        return Err(MetadataError::CorruptObject {
            path: descriptor_path.to_string(),
            reason: "shallow closure descriptor digest does not match its destination".to_owned(),
        });
    }
    let mut provided = BTreeSet::new();
    for entry in &write.entries {
        if !descriptor
            .entries
            .iter()
            .any(|reference| reference.hash == entry.reference.hash)
        {
            return Err(MetadataError::CorruptObject {
                path: entry.reference.path.clone(),
                reason: "shallow closure entry is absent from its descriptor".to_owned(),
            });
        }
        let path = router.repo_path(&entry.reference.path);
        let expected = decode_hash(&entry.reference.hash, path.as_ref())?;
        match store.head(&path).await {
            Ok(_) => {
                store.verify(&path, &expected).await?;
            }
            Err(crab_storage::StorageError::NotFound { .. }) => {
                store
                    .put(&path, bytes::Bytes::copy_from_slice(&entry.bytes))
                    .await
                    .map_err(MetadataError::from)?;
            }
            Err(error) => return Err(MetadataError::from(error)),
        }
        provided.insert(entry.reference.hash.clone());
    }
    for reference in &descriptor.entries {
        if provided.contains(&reference.hash) {
            continue;
        }
        let path = router.repo_path(&reference.path);
        match store.head(&path).await {
            Ok(metadata) if metadata.size == reference.bytes => {}
            Ok(metadata) => {
                return Err(MetadataError::CorruptObject {
                    path: path.to_string(),
                    reason: format!(
                        "shallow closure entry length mismatch: expected {}, got {}",
                        reference.bytes, metadata.size
                    ),
                });
            }
            Err(crab_storage::StorageError::NotFound { .. }) => {
                return Err(MetadataError::CorruptObject {
                    path: path.to_string(),
                    reason: "shallow closure entry is missing".to_owned(),
                });
            }
            Err(error) => return Err(MetadataError::from(error)),
        }
    }
    match store.get_with_etag(&descriptor_path).await {
        Ok((bytes, _)) if bytes == write.descriptor_bytes => Ok(()),
        Ok(_) => Err(MetadataError::CorruptObject {
            path: descriptor_path.to_string(),
            reason: "existing shallow closure descriptor differs from the requested generation"
                .to_owned(),
        }),
        Err(crab_storage::StorageError::NotFound { .. }) => store
            .put(
                &descriptor_path,
                bytes::Bytes::copy_from_slice(&write.descriptor_bytes),
            )
            .await
            .map_err(MetadataError::from),
        Err(error) => Err(MetadataError::from(error)),
    }
}

fn validate_entry_reference(reference: &ShallowClosureEntryRef) -> Result<()> {
    validate_sha1(
        &reference.tip,
        "shallow closure entry tip",
        "shallow closure",
    )?;
    if reference.depth == 0 || reference.bytes < ENTRY_HEADER_BYTES as u64 {
        return Err(corrupt("shallow closure entry reference is invalid"));
    }
    validate_content_hash(
        &reference.hash,
        "shallow closure entry hash",
        "shallow closure",
    )?;
    let expected_path = shallow_closure_entry_relative_path(&reference.hash);
    if reference.path != expected_path {
        return Err(corrupt(
            "shallow closure entry path is not content addressed",
        ));
    }
    if reference.object_count == 0 || reference.object_count as usize > MAX_OBJECTS_PER_ENTRY {
        return Err(corrupt("shallow closure entry object count is invalid"));
    }
    Ok(())
}

fn validate_entry(entry: &ShallowClosureEntry, path: &str) -> Result<()> {
    if entry.depth == 0 || entry.object_ids.is_empty() {
        return Err(corrupt_at(path, "shallow closure entry is empty"));
    }
    if entry.object_ids.len() > MAX_OBJECTS_PER_ENTRY {
        return Err(corrupt_at(
            path,
            "shallow closure entry has too many objects",
        ));
    }
    let mut previous = None;
    for oid in &entry.object_ids {
        if previous.is_some_and(|value| value >= oid) {
            return Err(corrupt_at(
                path,
                "shallow closure object IDs are not sorted and unique",
            ));
        }
        previous = Some(oid);
    }
    if entry.object_ids.binary_search(&entry.tip).is_err() {
        return Err(corrupt_at(
            path,
            "shallow closure entry does not contain its tip",
        ));
    }
    previous = None;
    for oid in &entry.shallow {
        if previous.is_some_and(|value| value >= oid) {
            return Err(corrupt_at(
                path,
                "shallow closure boundary IDs are not sorted and unique",
            ));
        }
        previous = Some(oid);
        if entry.object_ids.binary_search(oid).is_err() {
            return Err(corrupt_at(
                path,
                "shallow closure boundary is absent from its object set",
            ));
        }
    }
    Ok(())
}

fn encode_shallow_closure_entry(entry: &ShallowClosureEntry) -> Result<Vec<u8>> {
    let mut normalized = entry.clone();
    normalized.object_ids.sort_unstable();
    normalized.object_ids.dedup();
    normalized.shallow.sort_unstable();
    normalized.shallow.dedup();
    validate_entry(&normalized, "shallow closure entry")?;
    let object_count = u32::try_from(normalized.object_ids.len())
        .map_err(|_| corrupt("shallow closure object count exceeds u32"))?;
    let boundary_count = u32::try_from(normalized.shallow.len())
        .map_err(|_| corrupt("shallow closure boundary count exceeds u32"))?;
    let ids = normalized
        .object_ids
        .len()
        .checked_add(normalized.shallow.len())
        .ok_or_else(|| corrupt("shallow closure entry count overflows"))?;
    let capacity = ENTRY_HEADER_BYTES
        .checked_add(
            ids.checked_mul(20)
                .ok_or_else(|| corrupt("shallow closure entry size overflows"))?,
        )
        .ok_or_else(|| corrupt("shallow closure entry size overflows"))?;
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(ENTRY_MAGIC);
    bytes.extend_from_slice(&ENTRY_VERSION.to_le_bytes());
    bytes.extend_from_slice(&normalized.tip);
    bytes.extend_from_slice(&normalized.depth.to_le_bytes());
    bytes.extend_from_slice(&object_count.to_le_bytes());
    bytes.extend_from_slice(&boundary_count.to_le_bytes());
    for oid in normalized.object_ids.iter().chain(&normalized.shallow) {
        bytes.extend_from_slice(oid);
    }
    Ok(bytes)
}

fn sha1_hex(oid: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(40);
    for byte in oid {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "shallow closure".to_owned(),
        reason: reason.into(),
    }
}

fn corrupt_at(path: &str, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn read_u32(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<u32> {
    let end = cursor
        .checked_add(4)
        .ok_or_else(|| corrupt_at(path, "shallow closure cursor overflow"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| corrupt_at(path, "shallow closure entry is truncated"))?;
    *cursor = end;
    Ok(u32::from_le_bytes(value.try_into().map_err(|_| {
        corrupt_at(path, "shallow closure entry integer has invalid width")
    })?))
}

fn read_oid(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<[u8; 20]> {
    let end = cursor
        .checked_add(20)
        .ok_or_else(|| corrupt_at(path, "shallow closure cursor overflow"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| corrupt_at(path, "shallow closure entry is truncated"))?;
    *cursor = end;
    value
        .try_into()
        .map_err(|_| corrupt_at(path, "invalid shallow closure object ID"))
}

#[cfg(feature = "storage")]
fn decode_hash(value: &str, path: &str) -> Result<[u8; 32]> {
    blake3::Hash::from_hex(value)
        .map(|hash| *hash.as_bytes())
        .map_err(|error| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: format!("invalid content hash: {error}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(value: u8) -> [u8; 20] {
        [value; 20]
    }

    fn hash(value: u8) -> String {
        format!("{value:02x}").repeat(32)
    }

    #[test]
    fn entry_round_trip_normalizes_object_order() {
        let entry = ShallowClosureEntry {
            tip: oid(3),
            depth: 10,
            object_ids: vec![oid(3), oid(1), oid(2), oid(2)],
            shallow: vec![oid(2), oid(2)],
        };
        let bytes = encode_shallow_closure_entry(&entry).expect("encode entry");
        let decoded = decode_shallow_closure_entry(&bytes, "entry").expect("decode entry");
        assert_eq!(decoded.object_ids, vec![oid(1), oid(2), oid(3)]);
        assert_eq!(decoded.shallow, vec![oid(2)]);
    }

    #[test]
    fn descriptor_rejects_stale_entry_path() {
        let descriptor = ShallowClosureDescriptor {
            version: 1,
            generation: 1,
            pack_index_hash: hash(1),
            git_validation_digest: hash(2),
            entries: vec![ShallowClosureEntryRef {
                tip: format!("{:x}", 1).repeat(40),
                depth: 1,
                hash: hash(3),
                path: "metadata/shallow-closure/entries/not-the-hash.bin".to_owned(),
                bytes: ENTRY_HEADER_BYTES as u64,
                object_count: 1,
                boundary_count: 0,
            }],
        };
        assert!(descriptor.validate().is_err());
    }

    #[test]
    fn write_contains_content_addressed_entries() {
        let write = build_shallow_closure_write(
            1,
            hash(1),
            hash(2),
            vec![ShallowClosureEntry {
                tip: oid(1),
                depth: 1,
                object_ids: vec![oid(1)],
                shallow: vec![],
            }],
        )
        .expect("build write");
        assert_eq!(write.entries.len(), 1);
        let reference = &write.entries[0].reference;
        assert!(reference.path.ends_with(&format!("{}.bin", reference.hash)));
        let decoded = serde_json::from_slice::<ShallowClosureDescriptor>(&write.descriptor_bytes)
            .expect("descriptor");
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn rebind_preserves_content_addressed_entries() {
        let write = build_shallow_closure_write(
            1,
            hash(1),
            hash(2),
            vec![ShallowClosureEntry {
                tip: oid(1),
                depth: 1,
                object_ids: vec![oid(1)],
                shallow: vec![],
            }],
        )
        .expect("build write");
        let descriptor = decode_shallow_closure_descriptor(&write.descriptor_bytes, "descriptor")
            .expect("decode descriptor");

        let rebound =
            rebind_shallow_closure_write(&descriptor, 2, hash(3), hash(4)).expect("rebind write");
        assert!(rebound.entries.is_empty());
        let decoded = decode_shallow_closure_descriptor(&rebound.descriptor_bytes, "descriptor")
            .expect("decode rebound descriptor");
        assert_eq!(decoded.generation, 2);
        assert_eq!(decoded.pack_index_hash, hash(3));
        assert_eq!(decoded.git_validation_digest, hash(4));
        assert_eq!(decoded.entries, descriptor.entries);
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn storage_round_trip_rejects_a_stale_descriptor_and_corrupt_entry() {
        let store =
            crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
        let router = crab_storage::StoreLayout::new(store.clone(), "org/repo".to_owned());
        let write = build_shallow_closure_write(
            7,
            hash(1),
            hash(2),
            vec![ShallowClosureEntry {
                tip: oid(1),
                depth: 1,
                object_ids: vec![oid(1), oid(2)],
                shallow: vec![oid(1)],
            }],
        )
        .expect("build write");
        upload_shallow_closure(&store, &router, &hash(2), &write)
            .await
            .expect("upload closure");
        let rebound = rebind_shallow_closure_write(&descriptor(&write), 8, hash(3), hash(4))
            .expect("rebind closure");
        upload_shallow_closure(&store, &router, &hash(4), &rebound)
            .await
            .expect("upload rebound closure");
        let descriptor = load_shallow_closure_descriptor(
            &store,
            &router,
            &hash(2),
            7,
            &hash(1),
            DEFAULT_MAX_SHALLOW_CLOSURE_DESCRIPTOR_BYTES,
        )
        .await
        .expect("load descriptor")
        .expect("descriptor exists");
        assert!(
            load_shallow_closure_descriptor(
                &store,
                &router,
                &hash(2),
                8,
                &hash(1),
                DEFAULT_MAX_SHALLOW_CLOSURE_DESCRIPTOR_BYTES,
            )
            .await
            .is_err()
        );

        let reference = &descriptor.entries[0];
        store
            .put_overwrite(
                &router.repo_path(&reference.path),
                bytes::Bytes::from_static(b"corrupt"),
            )
            .await
            .expect("corrupt entry fixture");
        assert!(
            load_shallow_closure_entry(
                &store,
                &router,
                reference,
                DEFAULT_MAX_SHALLOW_CLOSURE_ENTRY_BYTES,
            )
            .await
            .is_err()
        );
    }

    fn descriptor(write: &ShallowClosureWrite) -> ShallowClosureDescriptor {
        decode_shallow_closure_descriptor(&write.descriptor_bytes, "descriptor")
            .expect("decode descriptor")
    }
}
