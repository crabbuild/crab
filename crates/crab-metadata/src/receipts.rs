//! Durable-origin and manifest-generation proof records.

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};

pub const RECEIPT_SCHEMA_VERSION: u32 = 2;
pub const COMMITTED_CHUNK_PLACEMENT_ENCODED_LEN: usize = 140;

fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Digest the protected-push ref edit set in canonical destination order.
#[must_use]
pub fn protected_ref_edit_digest(updates: &[(String, Option<String>, String)]) -> [u8; 32] {
    let mut updates = updates.to_vec();
    updates.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab protected ref edits v1\0");
    for (name, old, new) in updates {
        field(&mut hasher, name.as_bytes());
        field(&mut hasher, old.as_deref().unwrap_or_default().as_bytes());
        field(&mut hasher, new.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Digest the protected-push connectivity frontier.
#[must_use]
pub fn protected_connectivity_digest(new_oids: &[String]) -> [u8; 32] {
    let mut oids = new_oids.to_vec();
    oids.sort_unstable();
    oids.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab push connectivity v1\0");
    for oid in oids {
        field(&mut hasher, oid.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Digest the complete candidate shard set in canonical order.
#[must_use]
pub fn committed_shard_set_digest(shard_hashes: &[String]) -> [u8; 32] {
    let mut hashes = shard_hashes.to_vec();
    hashes.sort_unstable();
    hashes.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab push shard set v1\0");
    for hash in hashes {
        field(&mut hasher, hash.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Proof that a content-addressed object exists in the canonical origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginReceipt {
    pub schema_version: u32,
    pub namespace: String,
    pub object_key: String,
    pub content_hash: [u8; 32],
    pub payload_digest: [u8; 32],
    pub size: u64,
    pub etag: Option<String>,
    pub object_version: Option<String>,
    pub proven_at_unix_secs: u64,
}

impl OriginReceipt {
    #[must_use]
    pub fn new(
        namespace: String,
        object_key: String,
        content_hash: [u8; 32],
        payload_digest: [u8; 32],
        size: u64,
        etag: Option<String>,
        object_version: Option<String>,
    ) -> Self {
        let proven_at_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |duration| duration.as_secs().max(1));
        Self {
            schema_version: RECEIPT_SCHEMA_VERSION,
            namespace,
            object_key,
            content_hash,
            payload_digest,
            size,
            etag,
            object_version,
            proven_at_unix_secs,
        }
    }

    pub fn validate(
        &self,
        namespace: &str,
        object_key: &str,
        content_hash: [u8; 32],
        size: u64,
    ) -> Result<()> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION {
            return Err(invalid_receipt(format!(
                "unsupported origin receipt schema {}",
                self.schema_version
            )));
        }
        if self.namespace != namespace
            || self.object_key != object_key
            || self.content_hash != content_hash
            || self.payload_digest == [0; 32]
            || self.size != size
            || self.proven_at_unix_secs == 0
        {
            return Err(invalid_receipt(
                "origin receipt does not match the canonical object identity".to_owned(),
            ));
        }
        Ok(())
    }

    /// Stable identifier used by compact chunk placements.
    #[must_use]
    pub fn proof_id(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab origin proof v2\0");
        hasher.update(&self.schema_version.to_le_bytes());
        field(&mut hasher, self.namespace.as_bytes());
        field(&mut hasher, self.object_key.as_bytes());
        hasher.update(&self.content_hash);
        hasher.update(&self.payload_digest);
        hasher.update(&self.size.to_le_bytes());
        field(
            &mut hasher,
            self.etag.as_deref().unwrap_or_default().as_bytes(),
        );
        field(
            &mut hasher,
            self.object_version
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update(&self.proven_at_unix_secs.to_le_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// Repository-shard generation that roots committed chunk placements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAnchor {
    pub schema_version: u32,
    pub source_repo_prefix: String,
    pub source_shard_hash: [u8; 32],
    pub committed_generation: u64,
    pub shard_index_hash: [u8; 32],
    pub gc_registry_generation: u64,
}

impl SourceAnchor {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION
            || self.source_repo_prefix.is_empty()
            || self.source_shard_hash == [0; 32]
            || self.committed_generation == 0
            || self.shard_index_hash == [0; 32]
            || self.gc_registry_generation == 0
        {
            return Err(invalid_receipt(
                "source anchor is incomplete or uses an unsupported schema".to_owned(),
            ));
        }
        Ok(())
    }

    /// Stable identifier referenced by compact chunk placements.
    #[must_use]
    pub fn anchor_id(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab source anchor v2\0");
        hasher.update(&self.schema_version.to_le_bytes());
        field(&mut hasher, self.source_repo_prefix.as_bytes());
        hasher.update(&self.source_shard_hash);
        hasher.update(&self.committed_generation.to_le_bytes());
        hasher.update(&self.shard_index_hash);
        hasher.update(&self.gc_registry_generation.to_le_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// Compact persistent placement referencing xorb proof and source-root records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedChunkPlacement {
    pub schema_version: u32,
    pub chunk_hash: [u8; 32],
    pub xorb_hash: [u8; 32],
    pub chunk_index: u32,
    pub uncompressed_size: u32,
    pub origin_proof_id: [u8; 32],
    pub source_anchor_id: [u8; 32],
}

impl CommittedChunkPlacement {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION
            || self.chunk_hash == [0; 32]
            || self.xorb_hash == [0; 32]
            || self.uncompressed_size == 0
            || self.origin_proof_id == [0; 32]
            || self.source_anchor_id == [0; 32]
        {
            return Err(invalid_receipt(
                "committed chunk placement is incomplete or uses an unsupported schema".to_owned(),
            ));
        }
        Ok(())
    }

    /// Encode the placement using the fixed-width remote-index format.
    pub fn encode(&self) -> Result<[u8; COMMITTED_CHUNK_PLACEMENT_ENCODED_LEN]> {
        self.validate()?;
        let mut encoded = [0_u8; COMMITTED_CHUNK_PLACEMENT_ENCODED_LEN];
        encoded[0..4].copy_from_slice(&self.schema_version.to_le_bytes());
        encoded[4..36].copy_from_slice(&self.chunk_hash);
        encoded[36..68].copy_from_slice(&self.xorb_hash);
        encoded[68..72].copy_from_slice(&self.chunk_index.to_le_bytes());
        encoded[72..76].copy_from_slice(&self.uncompressed_size.to_le_bytes());
        encoded[76..108].copy_from_slice(&self.origin_proof_id);
        encoded[108..140].copy_from_slice(&self.source_anchor_id);
        Ok(encoded)
    }

    /// Decode one fixed-width placement, rejecting every retired format.
    pub fn decode(value: &[u8]) -> Result<Self> {
        if value.len() != COMMITTED_CHUNK_PLACEMENT_ENCODED_LEN {
            return Err(invalid_receipt(format!(
                "committed chunk placement has {} bytes, expected {COMMITTED_CHUNK_PLACEMENT_ENCODED_LEN}",
                value.len()
            )));
        }
        let mut schema_version = [0_u8; 4];
        schema_version.copy_from_slice(&value[0..4]);
        let mut chunk_hash = [0_u8; 32];
        chunk_hash.copy_from_slice(&value[4..36]);
        let mut xorb_hash = [0_u8; 32];
        xorb_hash.copy_from_slice(&value[36..68]);
        let mut chunk_index = [0_u8; 4];
        chunk_index.copy_from_slice(&value[68..72]);
        let mut uncompressed_size = [0_u8; 4];
        uncompressed_size.copy_from_slice(&value[72..76]);
        let mut origin_proof_id = [0_u8; 32];
        origin_proof_id.copy_from_slice(&value[76..108]);
        let mut source_anchor_id = [0_u8; 32];
        source_anchor_id.copy_from_slice(&value[108..140]);
        let placement = Self {
            schema_version: u32::from_le_bytes(schema_version),
            chunk_hash,
            xorb_hash,
            chunk_index: u32::from_le_bytes(chunk_index),
            uncompressed_size: u32::from_le_bytes(uncompressed_size),
            origin_proof_id,
            source_anchor_id,
        };
        placement.validate()?;
        Ok(placement)
    }

    /// Validate the shared proof records and their manifest visibility.
    pub fn validate_proof_records(
        &self,
        origin: &OriginReceipt,
        source: &SourceAnchor,
        base_generation: u64,
        shard_index_hash: [u8; 32],
    ) -> Result<()> {
        self.validate()?;
        source.validate()?;
        if self.origin_proof_id != origin.proof_id() || self.source_anchor_id != source.anchor_id()
        {
            return Err(invalid_receipt(
                "compact chunk placement references mismatched proof records".to_owned(),
            ));
        }
        if source.committed_generation > base_generation
            || (source.committed_generation == base_generation
                && source.shard_index_hash != shard_index_hash)
        {
            return Err(invalid_receipt(
                "committed chunk placement is not visible in the base generation".to_owned(),
            ));
        }
        if origin.namespace.is_empty() || origin.object_key.is_empty() || origin.size == 0 {
            return Err(invalid_receipt(
                "committed chunk placement has an incomplete origin identity".to_owned(),
            ));
        }
        origin.validate(
            &origin.namespace,
            &origin.object_key,
            self.xorb_hash,
            origin.size,
        )
    }

    /// Stable identifier used in immutable committed-chunk keys.
    #[must_use]
    pub fn placement_id(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab committed chunk placement v2\0");
        hasher.update(&self.schema_version.to_le_bytes());
        hasher.update(&self.chunk_hash);
        hasher.update(&self.xorb_hash);
        hasher.update(&self.chunk_index.to_le_bytes());
        hasher.update(&self.uncompressed_size.to_le_bytes());
        hasher.update(&self.origin_proof_id);
        hasher.update(&self.source_anchor_id);
        *hasher.finalize().as_bytes()
    }
}

/// Origin proof plus the committed shard and GC-root facts for one chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedChunkReceipt {
    pub schema_version: u32,
    pub chunk_hash: [u8; 32],
    pub xorb_hash: [u8; 32],
    pub chunk_index: u32,
    pub uncompressed_size: u32,
    pub origin: OriginReceipt,
    pub source_repo_prefix: String,
    pub source_shard_hash: [u8; 32],
    pub committed_generation: u64,
    pub shard_index_hash: [u8; 32],
    pub gc_registry_generation: u64,
}

impl CommittedChunkReceipt {
    #[must_use]
    pub fn source_anchor(&self) -> SourceAnchor {
        SourceAnchor {
            schema_version: self.schema_version,
            source_repo_prefix: self.source_repo_prefix.clone(),
            source_shard_hash: self.source_shard_hash,
            committed_generation: self.committed_generation,
            shard_index_hash: self.shard_index_hash,
            gc_registry_generation: self.gc_registry_generation,
        }
    }

    #[must_use]
    pub fn compact_placement(&self) -> CommittedChunkPlacement {
        let source = self.source_anchor();
        CommittedChunkPlacement {
            schema_version: self.schema_version,
            chunk_hash: self.chunk_hash,
            xorb_hash: self.xorb_hash,
            chunk_index: self.chunk_index,
            uncompressed_size: self.uncompressed_size,
            origin_proof_id: self.origin.proof_id(),
            source_anchor_id: source.anchor_id(),
        }
    }

    pub fn from_compact(
        placement: CommittedChunkPlacement,
        origin: OriginReceipt,
        source: SourceAnchor,
    ) -> Result<Self> {
        placement.validate()?;
        origin.validate(
            &origin.namespace,
            &origin.object_key,
            placement.xorb_hash,
            origin.size,
        )?;
        source.validate()?;
        if placement.origin_proof_id != origin.proof_id()
            || placement.source_anchor_id != source.anchor_id()
        {
            return Err(invalid_receipt(
                "compact chunk placement references mismatched proof records".to_owned(),
            ));
        }
        let receipt = Self {
            schema_version: placement.schema_version,
            chunk_hash: placement.chunk_hash,
            xorb_hash: placement.xorb_hash,
            chunk_index: placement.chunk_index,
            uncompressed_size: placement.uncompressed_size,
            origin,
            source_repo_prefix: source.source_repo_prefix,
            source_shard_hash: source.source_shard_hash,
            committed_generation: source.committed_generation,
            shard_index_hash: source.shard_index_hash,
            gc_registry_generation: source.gc_registry_generation,
        };
        receipt.validate(receipt.committed_generation, receipt.shard_index_hash)?;
        Ok(receipt)
    }

    pub fn validate(&self, base_generation: u64, shard_index_hash: [u8; 32]) -> Result<()> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION {
            return Err(invalid_receipt(format!(
                "unsupported committed chunk receipt schema {}",
                self.schema_version
            )));
        }
        if self.source_repo_prefix.is_empty()
            || self.chunk_hash == [0; 32]
            || self.xorb_hash == [0; 32]
            || self.uncompressed_size == 0
            || self.source_shard_hash == [0; 32]
            || self.committed_generation == 0
            || self.committed_generation > base_generation
        {
            return Err(invalid_receipt(
                "committed chunk receipt is not visible in the base generation".to_owned(),
            ));
        }
        if (self.committed_generation == base_generation
            && self.shard_index_hash != shard_index_hash)
            || self.shard_index_hash == [0; 32]
            || self.gc_registry_generation == 0
        {
            return Err(invalid_receipt(
                "committed chunk receipt is not anchored to the current shard index and GC registry"
                    .to_owned(),
            ));
        }
        self.origin.validate(
            &self.origin.namespace,
            &self.origin.object_key,
            self.xorb_hash,
            self.origin.size,
        )?;
        if self.origin.namespace.is_empty()
            || self.origin.object_key.is_empty()
            || self.origin.size == 0
        {
            return Err(invalid_receipt(
                "committed chunk receipt has an incomplete origin identity".to_owned(),
            ));
        }
        Ok(())
    }

    /// Validate that this receipt describes the queried placement.
    pub fn validate_placement(
        &self,
        chunk_hash: [u8; 32],
        xorb_hash: [u8; 32],
        chunk_index: u32,
        uncompressed_size: u32,
        base_generation: u64,
        shard_index_hash: [u8; 32],
    ) -> Result<()> {
        self.validate(base_generation, shard_index_hash)?;
        if self.chunk_hash != chunk_hash
            || self.xorb_hash != xorb_hash
            || self.chunk_index != chunk_index
            || self.uncompressed_size != uncompressed_size
        {
            return Err(invalid_receipt(
                "committed chunk receipt does not match the queried placement".to_owned(),
            ));
        }
        Ok(())
    }

    /// Stable content identifier used in the committed chunk key.
    #[must_use]
    pub fn receipt_id(&self) -> [u8; 32] {
        self.compact_placement().placement_id()
    }
}

/// Post-CAS acceleration receipt tied to the manifest's committed indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationIndexReceipt {
    pub schema_version: u32,
    pub generation: u64,
    pub shard_index_hash: [u8; 32],
    pub pack_index_hash: [u8; 32],
    pub file_index_digest: [u8; 32],
    pub git_object_locator_digest: [u8; 32],
}

impl GenerationIndexReceipt {
    pub fn validate(
        &self,
        generation: u64,
        shard_index_hash: [u8; 32],
        pack_index_hash: [u8; 32],
    ) -> Result<()> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION
            || self.generation == 0
            || self.generation != generation
            || self.shard_index_hash != shard_index_hash
            || self.pack_index_hash != pack_index_hash
            || self.file_index_digest == [0; 32]
            || self.git_object_locator_digest == [0; 32]
        {
            return Err(invalid_receipt(
                "generation-index receipt does not match the committed manifest".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Digest the canonical shard index from which file/chunk indexes rebuild.
#[must_use]
pub fn generation_file_index_digest(shard_index_hash: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab generation file index v1\0");
    hasher.update(&shard_index_hash);
    *hasher.finalize().as_bytes()
}

/// Digest the canonical pack index from which Git locators rebuild.
#[must_use]
pub fn generation_git_object_locator_digest(pack_index_hash: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab generation git object locator\0");
    hasher.update(&pack_index_hash);
    *hasher.finalize().as_bytes()
}

/// Base-bound dependency closure validated immediately before manifest CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushCommitReceipt {
    pub schema_version: u32,
    pub attempt_id: String,
    pub base_generation: u64,
    pub base_etag: Option<String>,
    pub ref_edit_digest: [u8; 32],
    pub git_object_set_digest: [u8; 32],
    pub file_recipe_set_digest: [u8; 32],
    pub xorb_proof_digest: [u8; 32],
    pub shard_set_digest: [u8; 32],
    pub candidate_pack_index_hash: [u8; 32],
    pub candidate_shard_index_hash: [u8; 32],
    pub gc_registry_generation: u64,
    pub connectivity_digest: [u8; 32],
    pub plan_digest: [u8; 32],
}

impl PushCommitReceipt {
    pub fn validate_base(&self, generation: u64, etag: Option<&str>) -> Result<()> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION
            || self.attempt_id.is_empty()
            || self.base_generation != generation
            || self.base_etag.as_deref() != etag
            || self.ref_edit_digest == [0; 32]
            || self.git_object_set_digest == [0; 32]
            || self.file_recipe_set_digest == [0; 32]
            || self.xorb_proof_digest == [0; 32]
            || self.shard_set_digest == [0; 32]
            || self.connectivity_digest == [0; 32]
            || self.plan_digest == [0; 32]
        {
            return Err(invalid_receipt(
                "push commit receipt is stale or incomplete for the manifest base".to_owned(),
            ));
        }
        Ok(())
    }
}

fn invalid_receipt(reason: String) -> MetadataError {
    MetadataError::CorruptObject {
        path: "metadata receipt".to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed_receipt() -> CommittedChunkReceipt {
        CommittedChunkReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            chunk_hash: [1; 32],
            xorb_hash: [2; 32],
            chunk_index: 0,
            uncompressed_size: 8,
            origin: OriginReceipt::new(
                "bucket".to_owned(),
                "xorb/aa".to_owned(),
                [2; 32],
                [9; 32],
                10,
                None,
                None,
            ),
            source_repo_prefix: "org/repo".to_owned(),
            source_shard_hash: [3; 32],
            committed_generation: 4,
            shard_index_hash: [5; 32],
            gc_registry_generation: 9,
        }
    }

    fn expanded_receipt_json(receipt: &CommittedChunkReceipt) -> serde_json::Value {
        serde_json::json!({
            "schema_version": receipt.schema_version,
            "chunk_hash": receipt.chunk_hash,
            "xorb_hash": receipt.xorb_hash,
            "chunk_index": receipt.chunk_index,
            "uncompressed_size": receipt.uncompressed_size,
            "origin": receipt.origin,
            "source_repo_prefix": receipt.source_repo_prefix,
            "source_shard_hash": receipt.source_shard_hash,
            "committed_generation": receipt.committed_generation,
            "shard_index_hash": receipt.shard_index_hash,
            "gc_registry_generation": receipt.gc_registry_generation,
        })
    }

    #[test]
    fn origin_receipt_is_bound_to_canonical_identity() {
        let receipt = OriginReceipt::new(
            "bucket".to_owned(),
            "xorb/aa".to_owned(),
            [1; 32],
            [9; 32],
            42,
            Some("etag".to_owned()),
            None,
        );

        receipt.validate("bucket", "xorb/aa", [1; 32], 42).unwrap();
        assert!(receipt.validate("bucket", "xorb/bb", [1; 32], 42).is_err());
    }

    #[test]
    fn committed_chunk_receipt_rejects_future_or_unrooted_proof() {
        let mut receipt = committed_receipt();
        receipt.validate(4, [5; 32]).unwrap();
        assert!(receipt.validate(3, [5; 32]).is_err());
        receipt.gc_registry_generation = 0;
        assert!(receipt.validate(4, [5; 32]).is_err());
    }

    #[test]
    fn compact_chunk_records_amortize_repeated_proof_payloads() {
        let receipt = committed_receipt();
        let placement = receipt.compact_placement();
        let source = receipt.source_anchor();
        let compact_bytes = placement.encode().unwrap().len() * 100
            + serde_json::to_vec(&receipt.origin).unwrap().len()
            + serde_json::to_vec(&source).unwrap().len();
        let expanded_bytes = serde_json::to_vec(&expanded_receipt_json(&receipt))
            .unwrap()
            .len()
            * 100;

        assert!(compact_bytes * 10 < expanded_bytes * 7);
    }

    #[test]
    fn compact_chunk_rejects_mismatched_origin_proof() {
        let receipt = committed_receipt();
        let placement = receipt.compact_placement();
        let source = receipt.source_anchor();
        let mut wrong_origin = receipt.origin;
        wrong_origin.object_key = "xorb/different".to_owned();

        assert!(CommittedChunkReceipt::from_compact(placement, wrong_origin, source).is_err());
    }

    #[test]
    fn compact_placement_binary_round_trip_has_fixed_size() {
        let placement = committed_receipt().compact_placement();

        let encoded = placement.encode().unwrap();
        let decoded = CommittedChunkPlacement::decode(&encoded).unwrap();

        assert_eq!(encoded.len(), COMMITTED_CHUNK_PLACEMENT_ENCODED_LEN);
        assert_eq!(decoded, placement);
    }

    #[test]
    fn compact_chunk_decoder_rejects_expanded_json_shape() {
        let expanded = serde_json::to_vec(&expanded_receipt_json(&committed_receipt())).unwrap();

        assert!(CommittedChunkPlacement::decode(&expanded).is_err());
    }

    fn generation_receipt(locator_digest: [u8; 32]) -> GenerationIndexReceipt {
        GenerationIndexReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            generation: 3,
            shard_index_hash: [1; 32],
            pack_index_hash: [2; 32],
            file_index_digest: [3; 32],
            git_object_locator_digest: locator_digest,
        }
    }

    #[test]
    fn locator_digest_uses_the_hard_cut_domain() {
        let pack_index_hash = [7; 32];
        let mut expected = blake3::Hasher::new();
        expected.update(b"crab generation git object locator\0");
        expected.update(&pack_index_hash);

        assert_eq!(
            generation_git_object_locator_digest(pack_index_hash),
            *expected.finalize().as_bytes()
        );
    }

    #[test]
    fn generation_receipt_serializes_only_locator_digest_name() {
        let json = serde_json::to_value(generation_receipt([4; 32])).unwrap();
        let object = json.as_object().unwrap();

        assert!(object.contains_key("git_object_locator_digest"));
    }

    #[test]
    fn generation_receipt_requires_nonzero_locator_digest() {
        let error = generation_receipt([0; 32])
            .validate(3, [1; 32], [2; 32])
            .unwrap_err();

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }
}
