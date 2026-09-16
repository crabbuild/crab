use std::collections::BTreeMap;

use bytes::Bytes;
use crab_xet::hash::MerkleHash;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

const POINTER_CATALOG_VERSION: u32 = 1;

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
        delta.validate(false, false)?;
        for (hash, entry) in &delta.xorbs {
            self.insert_xorb(hash.clone(), entry.clone())?;
        }
        for (hash, entry) in &delta.shards {
            self.insert_shard(hash.clone(), entry.clone())?;
        }
        for (hash, entry) in &delta.files {
            match self.files.get(hash) {
                Some(existing) if existing.size != entry.size => {
                    return Err(contract_error(format!(
                        "file {hash} has conflicting declared sizes"
                    )));
                }
                _ => {
                    self.files.insert(hash.clone(), entry.clone());
                }
            }
        }
        self.validate(true, false)
    }

    /// Canonically encode and validate this catalog.
    pub fn encode(&self) -> Result<Bytes> {
        self.encode_with_validation(true)
    }

    /// Canonically encode a delta whose dependencies may come from its base catalog.
    pub fn encode_delta(&self) -> Result<Bytes> {
        self.encode_with_validation(false)
    }

    fn encode_with_validation(&self, require_complete_closure: bool) -> Result<Bytes> {
        self.validate(require_complete_closure, false)?;
        serde_json::to_vec(self)
            .map(Bytes::from)
            .map_err(|source| MetadataError::Internal(format!("pointer catalog encode: {source}")))
    }

    /// Decode canonical bytes and validate their complete dependency closure.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
                path: "capsule-protocol pointer catalog".to_owned(),
                reason: format!("invalid JSON: {source}"),
            })?;
        let canonical = serde_json::to_vec(&catalog).map_err(|source| {
            MetadataError::Internal(format!("pointer catalog re-encode: {source}"))
        })?;
        if canonical != bytes {
            return Err(corrupt("catalog is not canonically encoded"));
        }
        catalog.validate(true, true)?;
        Ok(catalog)
    }

    /// Decode a canonical delta and defer base-dependent closure checks to apply.
    pub fn decode_delta(bytes: &[u8]) -> Result<Self> {
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
                path: "capsule-protocol pointer catalog".to_owned(),
                reason: format!("invalid JSON: {source}"),
            })?;
        let canonical = serde_json::to_vec(&catalog).map_err(|source| {
            MetadataError::Internal(format!("pointer catalog re-encode: {source}"))
        })?;
        if canonical != bytes {
            return Err(corrupt("catalog is not canonically encoded"));
        }
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
    if map.get(&hash).is_some_and(|existing| existing != &entry) {
        return Err(contract_error(format!(
            "{kind} {hash} has conflicting descriptors"
        )));
    }
    map.insert(hash, entry);
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
