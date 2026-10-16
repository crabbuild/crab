use std::collections::BTreeMap;

use bytes::Bytes;
use crab_xet::hash::MerkleHash;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

const POINTER_CATALOG_VERSION: u32 = 1;

#[cfg(feature = "storage")]
pub(super) mod inspection;
pub(in crate::capsule_protocol) mod object;
pub(super) mod paged;
#[cfg(feature = "storage")]
mod relations;
#[cfg(feature = "storage")]
pub(super) mod spill;
#[cfg(feature = "storage")]
pub use inspection::{
    PointerCatalogCounts, PointerCatalogEntry, PointerCatalogInspection, PointerCatalogObject,
    PointerCatalogShardInspection, inspect_pointer_catalog,
};
pub use paged::MAX_CATALOG_BYTES;
#[cfg(feature = "storage")]
pub use paged::PointerCatalogLookup;

/// One file identity and the canonical shard that reconstructs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCatalogEntry {
    size: u64,
    shard_hash: String,
}

impl FileCatalogEntry {
    /// Bind one file identity to a complete reconstruction shard.
    #[must_use]
    pub fn new(size: u64, shard_hash: impl Into<String>) -> Self {
        Self {
            size,
            shard_hash: shard_hash.into(),
        }
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn shard_hash(&self) -> &str {
        &self.shard_hash
    }
}

/// One ordered chunk entry in a canonical xorb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XorbChunkEntry {
    hash: String,
    uncompressed_size: u32,
}

impl XorbChunkEntry {
    #[must_use]
    pub fn new(hash: impl Into<String>, uncompressed_size: u32) -> Self {
        Self {
            hash: hash.into(),
            uncompressed_size,
        }
    }

    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    #[must_use]
    pub fn uncompressed_size(&self) -> u32 {
        self.uncompressed_size
    }
}

/// Authenticated metadata needed to reuse a canonical external xorb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XorbCatalogEntry {
    encoded_size: u64,
    body_digest: String,
    chunks: Vec<XorbChunkEntry>,
}

impl XorbCatalogEntry {
    #[must_use]
    pub fn new(
        encoded_size: u64,
        body_digest: impl Into<String>,
        chunks: Vec<XorbChunkEntry>,
    ) -> Self {
        Self {
            encoded_size,
            body_digest: body_digest.into(),
            chunks,
        }
    }

    #[must_use]
    pub fn encoded_size(&self) -> u64 {
        self.encoded_size
    }

    #[must_use]
    pub fn body_digest(&self) -> &str {
        &self.body_digest
    }

    #[must_use]
    pub fn chunks(&self) -> &[XorbChunkEntry] {
        &self.chunks
    }
}

/// One immutable shard and its complete external xorb closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardCatalogEntry {
    encoded_size: u64,
    xorb_hashes: Vec<String>,
}

impl ShardCatalogEntry {
    #[must_use]
    pub fn new(encoded_size: u64, xorb_hashes: Vec<String>) -> Self {
        Self {
            encoded_size,
            xorb_hashes,
        }
    }

    #[must_use]
    pub fn encoded_size(&self) -> u64 {
        self.encoded_size
    }

    #[must_use]
    pub fn xorb_hashes(&self) -> &[String] {
        &self.xorb_hashes
    }
}

/// Complete or incremental authenticated catalog for external pointer data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerCatalog {
    version: u32,
    files: BTreeMap<String, FileCatalogEntry>,
    shards: BTreeMap<String, ShardCatalogEntry>,
    xorbs: BTreeMap<String, XorbCatalogEntry>,
}

impl Default for PointerCatalog {
    fn default() -> Self {
        Self {
            version: POINTER_CATALOG_VERSION,
            files: BTreeMap::new(),
            shards: BTreeMap::new(),
            xorbs: BTreeMap::new(),
        }
    }
}

/// Assemble a captured visible catalog without assuming dependency order across refs.
///
/// File bindings retain capture order; immutable conflicts fail immediately.
/// Dependency completeness is required before the catalog can leave the assembly.
#[must_use]
pub struct PointerCatalogAssembly {
    catalog: PointerCatalog,
}

impl PointerCatalogAssembly {
    /// Start with the captured checkpoint catalog, or an empty catalog before checkpointing.
    pub fn new(catalog: PointerCatalog) -> Self {
        Self { catalog }
    }

    /// Add one ordered visible delta, validating its rows and immutable conflicts.
    ///
    /// Discard the assembly if a delta fails. Dependencies may come from another
    /// visible ref; callers must finish before using the resulting catalog.
    pub fn apply(&mut self, delta: &PointerCatalog) -> Result<()> {
        self.catalog.merge_delta(delta)
    }

    /// Return an ordered delta whose missing dependencies must be resolved from its base.
    pub fn finish_delta(self) -> Result<PointerCatalog> {
        self.catalog.validate(false, false)?;
        Ok(self.catalog)
    }

    /// Return the captured catalog only if its complete dependency closure is valid.
    pub fn finish(self) -> Result<PointerCatalog> {
        self.catalog.validate(true, false)?;
        Ok(self.catalog)
    }
}

pub(in crate::capsule_protocol) fn hash(value: &str) -> Result<[u8; 32]> {
    let bytes = value.as_bytes();
    // Binary keys cannot distinguish hex aliases. Reject them instead of silently
    // merging two descriptors that were distinct in the caller's map.
    if bytes.len() != 64
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(byte))
    {
        return Err(contract_error(
            "catalog hashes must be lowercase canonical hex",
        ));
    }
    // Encode text order directly: Xet's raw hash bytes reverse each u64 word.
    // Using those bytes as keys would change the original BTreeMap precedence.
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        _ => byte - b'a' + 10,
    };
    Ok(std::array::from_fn(|index| {
        (digit(bytes[index * 2]) << 4) | digit(bytes[index * 2 + 1])
    }))
}

fn hex(bytes: &[u8]) -> Result<String> {
    if bytes.len() != 32 {
        return Err(corrupt("catalog hash has an invalid byte length"));
    }
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(64);
    for byte in bytes {
        value.push(DIGITS[(byte >> 4) as usize] as char);
        value.push(DIGITS[(byte & 15) as usize] as char);
    }
    Ok(value)
}

// Bound unavoidable row payload before cloning caller-owned catalogs or recipes.
// Both packed deltas and object-paged checkpoints share this admission rule;
// page framing and actual encoded bytes are checked again by their writers.
pub(in crate::capsule_protocol) fn admit_catalog_rows(
    catalogs: &[PointerCatalog],
    maximum: u64,
) -> Result<()> {
    let bytes = catalogs.iter().try_fold(0u64, |sum, catalog| {
        let sum = sum.checked_add((catalog.files.len() as u64).checked_mul(76)?)?;
        let sum = catalog.xorbs.values().try_fold(sum, |sum, xorb| {
            sum.checked_add(76)?
                .checked_add((xorb.chunks.len() as u64).checked_mul(44)?)
        })?;
        catalog.shards.values().try_fold(sum, |sum, shard| {
            sum.checked_add(44)?
                .checked_add((shard.xorb_hashes.len() as u64).checked_mul(36)?)
        })
    });
    if bytes.is_none_or(|bytes| bytes > maximum.min(MAX_CATALOG_BYTES)) {
        return Err(contract_error(
            "catalog rows exceed their encoded byte bound",
        ));
    }
    Ok(())
}

impl PointerCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_file(
        &mut self,
        file_hash: impl Into<String>,
        entry: FileCatalogEntry,
    ) -> Result<()> {
        insert_consistent(&mut self.files, file_hash.into(), entry, "file")
    }

    pub fn insert_shard(
        &mut self,
        shard_hash: impl Into<String>,
        entry: ShardCatalogEntry,
    ) -> Result<()> {
        insert_consistent(&mut self.shards, shard_hash.into(), entry, "shard")
    }

    pub fn insert_xorb(
        &mut self,
        xorb_hash: impl Into<String>,
        entry: XorbCatalogEntry,
    ) -> Result<()> {
        insert_consistent(&mut self.xorbs, xorb_hash.into(), entry, "xorb")
    }

    /// Apply one publication delta, rejecting immutable identity conflicts.
    pub fn apply(&mut self, delta: &Self) -> Result<()> {
        self.merge_delta(delta)?;
        self.validate(true, false)
    }

    fn merge_delta(&mut self, delta: &Self) -> Result<()> {
        delta.validate(false, false)?;
        for (hash, entry) in &delta.xorbs {
            self.insert_xorb(hash.clone(), entry.clone())?;
        }
        for (hash, entry) in &delta.shards {
            self.insert_shard(hash.clone(), entry.clone())?;
        }
        for (hash, entry) in &delta.files {
            if let Some(existing) = self.files.get(hash) {
                validate_binding_size(hash, existing.size, entry.size)?;
            }
            self.files.insert(hash.clone(), entry.clone());
        }
        Ok(())
    }

    /// Encode this complete catalog as bounded authenticated page trees.
    ///
    /// Rejects incomplete dependencies, noncanonical hash keys and oversized catalogs.
    pub fn encode(&self) -> Result<Bytes> {
        self.encode_with_validation(true)
    }

    /// Encode paged delta metadata whose dependencies may come from its base catalog.
    pub fn encode_delta(&self) -> Result<Bytes> {
        self.encode_with_validation(false)
    }

    fn encode_with_validation(&self, require_complete_closure: bool) -> Result<Bytes> {
        self.validate(require_complete_closure, false)?;
        paged::encode(self)
    }

    /// Authenticate canonical pages and validate their complete dependency closure.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let catalog = paged::decode(bytes)?;
        catalog.validate(true, true)?;
        Ok(catalog)
    }

    /// Authenticate canonical delta pages, deferring base-owned closure checks to apply.
    pub fn decode_delta(bytes: &[u8]) -> Result<Self> {
        let catalog = paged::decode(bytes)?;
        catalog.validate(false, true)?;
        Ok(catalog)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.shards.is_empty() && self.xorbs.is_empty()
    }

    #[must_use]
    pub fn files(&self) -> &BTreeMap<String, FileCatalogEntry> {
        &self.files
    }

    #[must_use]
    pub fn shards(&self) -> &BTreeMap<String, ShardCatalogEntry> {
        &self.shards
    }

    #[must_use]
    pub fn xorbs(&self) -> &BTreeMap<String, XorbCatalogEntry> {
        &self.xorbs
    }

    fn validate(&self, require_complete_closure: bool, corrupt_input: bool) -> Result<()> {
        let failure = |reason: String| {
            if corrupt_input {
                corrupt(reason)
            } else {
                contract_error(reason)
            }
        };
        if self.version != POINTER_CATALOG_VERSION {
            return Err(failure(format!(
                "catalog version must be {POINTER_CATALOG_VERSION}"
            )));
        }
        for (hash, entry) in &self.xorbs {
            validate_hash(hash, "xorb", corrupt_input)?;
            validate_hash(&entry.body_digest, "xorb body digest", corrupt_input)?;
            if entry.encoded_size == 0 || entry.chunks.is_empty() {
                return Err(failure(format!("xorb {hash} is empty")));
            }
            for chunk in &entry.chunks {
                validate_hash(&chunk.hash, "chunk", corrupt_input)?;
                if chunk.uncompressed_size == 0 {
                    return Err(failure(format!("xorb {hash} contains an empty chunk")));
                }
            }
        }
        for (hash, entry) in &self.shards {
            validate_hash(hash, "shard", corrupt_input)?;
            if entry.encoded_size == 0 {
                return Err(failure(format!("shard {hash} is empty")));
            }
            if !entry.xorb_hashes.windows(2).all(|pair| pair[0] < pair[1]) {
                return Err(failure(format!(
                    "shard {hash} xorb closure is not sorted and unique"
                )));
            }
            for xorb_hash in &entry.xorb_hashes {
                validate_hash(xorb_hash, "shard xorb", corrupt_input)?;
                if require_complete_closure && !self.xorbs.contains_key(xorb_hash) {
                    return Err(failure(format!(
                        "shard {hash} references absent xorb {xorb_hash}"
                    )));
                }
            }
        }
        for (hash, entry) in &self.files {
            validate_hash(hash, "file", corrupt_input)?;
            validate_hash(&entry.shard_hash, "file shard", corrupt_input)?;
            if require_complete_closure && !self.shards.contains_key(&entry.shard_hash) {
                return Err(failure(format!(
                    "file {hash} references absent shard {}",
                    entry.shard_hash
                )));
            }
        }
        Ok(())
    }
}

fn insert_consistent<T: PartialEq>(
    map: &mut BTreeMap<String, T>,
    hash: String,
    entry: T,
    kind: &str,
) -> Result<()> {
    validate_consistent(map.get(&hash), &entry, &hash, kind)?;
    map.insert(hash, entry);
    Ok(())
}

fn validate_binding_size(hash: &str, previous: u64, incoming: u64) -> Result<()> {
    if previous != incoming {
        return Err(contract_error(format!(
            "file {hash} has conflicting declared sizes"
        )));
    }
    Ok(())
}

fn validate_consistent<T: PartialEq + ?Sized>(
    previous: Option<&T>,
    incoming: &T,
    hash: &str,
    kind: &str,
) -> Result<()> {
    if previous.is_some_and(|previous| previous != incoming) {
        return Err(contract_error(format!(
            "{kind} {hash} has conflicting descriptors"
        )));
    }
    Ok(())
}

fn validate_hash(value: &str, label: &str, corrupt_input: bool) -> Result<()> {
    validate_content_hash(value, label, "capsule-protocol pointer catalog").map_err(|error| {
        if corrupt_input {
            corrupt(error.to_string())
        } else {
            error
        }
    })?;
    MerkleHash::from_hex(value).map_err(|source| {
        let reason = format!("{label} hash is invalid: {source}");
        if corrupt_input {
            corrupt(reason)
        } else {
            contract_error(reason)
        }
    })?;
    Ok(())
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "pointer catalog",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol pointer catalog".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> PointerCatalog {
        let xorb = "1".repeat(64);
        let shard = "2".repeat(64);
        let file = "3".repeat(64);
        let mut catalog = PointerCatalog::new();
        catalog
            .insert_xorb(
                xorb.clone(),
                XorbCatalogEntry::new(
                    100,
                    "4".repeat(64),
                    vec![XorbChunkEntry::new("5".repeat(64), 9)],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(shard.clone(), ShardCatalogEntry::new(50, vec![xorb]))
            .unwrap();
        catalog
            .insert_file(file, FileCatalogEntry::new(9, shard))
            .unwrap();
        catalog
    }

    fn delta_with_base_xorb() -> PointerCatalog {
        let xorb = "1".repeat(64);
        let shard = "6".repeat(64);
        let file = "7".repeat(64);
        let mut delta = PointerCatalog::new();
        delta
            .insert_shard(shard.clone(), ShardCatalogEntry::new(60, vec![xorb]))
            .unwrap();
        delta
            .insert_file(file, FileCatalogEntry::new(10, shard))
            .unwrap();
        delta
    }

    #[test]
    fn catalog_round_trip_preserves_dependency_closure() {
        let expected = catalog();
        let encoded = expected.encode().unwrap();
        assert_eq!(PointerCatalog::decode(&encoded).unwrap(), expected);
    }

    #[test]
    fn catalog_rejects_missing_xorb_dependency() {
        let mut catalog = catalog();
        catalog.xorbs.clear();
        assert!(catalog.encode().is_err());
    }

    #[test]
    fn delta_round_trip_allows_base_owned_dependencies() {
        let expected = delta_with_base_xorb();
        let encoded = expected.encode_delta().unwrap();
        assert_eq!(PointerCatalog::decode_delta(&encoded).unwrap(), expected);
    }

    #[test]
    fn apply_resolves_dependencies_from_the_base_catalog() {
        let mut current = catalog();
        current.apply(&delta_with_base_xorb()).unwrap();
        assert!(current.files.contains_key(&"7".repeat(64)));
    }

    #[test]
    fn apply_rejects_dependency_absent_from_delta_and_base() {
        let mut current = PointerCatalog::new();
        assert!(current.apply(&delta_with_base_xorb()).is_err());
    }

    #[test]
    fn visible_assembly_rejects_unresolved_dependencies_at_finish() {
        let mut assembly = PointerCatalogAssembly::new(PointerCatalog::new());
        assembly.apply(&delta_with_base_xorb()).unwrap();
        assert!(assembly.finish().is_err());
    }

    #[test]
    fn apply_allows_new_file_mapping_but_not_new_size() {
        let mut current = catalog();
        let file = "3".repeat(64);
        let mut remap = catalog();
        remap
            .files
            .insert(file.clone(), FileCatalogEntry::new(9, "2".repeat(64)));
        current.apply(&remap).unwrap();
        remap
            .files
            .insert(file, FileCatalogEntry::new(10, "2".repeat(64)));
        assert!(current.apply(&remap).is_err());
    }
}
