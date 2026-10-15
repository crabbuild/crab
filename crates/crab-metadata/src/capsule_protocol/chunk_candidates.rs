//! Rebuildable chunk hints whose source authority is a capsule catalog.
#[cfg(any(test, feature = "remote-index"))]
use std::collections::BTreeMap;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::XorbCatalogEntry;
use crate::error::{MetadataError, Result};
#[cfg(any(test, feature = "remote-index"))]
use crate::receipts::RECEIPT_SCHEMA_VERSION;
use crate::receipts::{CommittedChunkPlacement, OriginReceipt};
use crab_xet::hash::MerkleHash;

pub use crate::key_codec::{capsule_chunk_anchor_key, capsule_chunk_head_key};

/// Capsule source and verified physical origin for one xorb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleChunkAnchor {
    pub source_repo_prefix: String,
    pub source_shard_hash: String,
    pub body_digest: String,
    pub origin: OriginReceipt,
}

impl CapsuleChunkAnchor {
    /// Decode bounded provenance and verify its immutable index identity.
    pub fn decode(id: &[u8; 32], bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 16 * 1024 {
            return Err(corrupt("capsule source anchor exceeds its byte bound"));
        }
        let anchor: Self = serde_json::from_slice(bytes)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if anchor.id()? != *id {
            return Err(corrupt("capsule source anchor digest differs"));
        }
        Ok(anchor)
    }

    /// Identify this immutable source and origin assertion.
    pub fn id(&self) -> Result<[u8; 32]> {
        if self.source_repo_prefix.len() > 4096
            || self.source_shard_hash.len() != 64
            || self.body_digest.len() != 64
        {
            return Err(corrupt("invalid capsule source identity"));
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if bytes.len() > 16 * 1024 {
            return Err(corrupt("capsule source anchor exceeds its byte bound"));
        }
        let mut hash = blake3::Hasher::new_derive_key("crab capsule chunk source v1");
        hash.update(&bytes);
        Ok(*hash.finalize().as_bytes())
    }

    /// Check an exact placement against its committed shard closure and origin assertion.
    pub fn validate_placement(
        &self,
        placement: &CommittedChunkPlacement,
        catalog: &super::PointerCatalog,
        object_key: &str,
    ) -> Result<()> {
        placement.validate()?;
        let xorb_hash = MerkleHash::from(placement.xorb_hash).hex();
        let rooted = catalog
            .shards()
            .get(&self.source_shard_hash)
            .is_some_and(|shard| shard.xorb_hashes().binary_search(&xorb_hash).is_ok());
        let entry = catalog
            .xorbs()
            .get(&xorb_hash)
            .ok_or_else(|| corrupt("capsule placement xorb is absent from source catalog"))?;
        self.validate_catalog_xorb(object_key, entry)?;
        let chunk = entry.chunks().get(placement.chunk_index as usize);
        if !rooted
            || placement.source_anchor_id != self.id()?
            || placement.origin_proof_id != self.origin.proof_id()
            || placement.xorb_hash != self.origin.content_hash
            || !chunk.is_some_and(|chunk| {
                chunk.hash() == MerkleHash::from(placement.chunk_hash).hex()
                    && chunk.uncompressed_size() == placement.uncompressed_size
            })
        {
            return Err(corrupt(
                "capsule placement differs from its source shard or origin",
            ));
        }
        Ok(())
    }

    /// Check the source catalog's physical descriptor before verifying origin availability.
    pub fn validate_catalog_xorb(&self, object_key: &str, entry: &XorbCatalogEntry) -> Result<()> {
        if self.source_repo_prefix.is_empty() || self.body_digest != entry.body_digest() {
            return Err(corrupt(
                "source catalog differs from the capsule chunk anchor",
            ));
        }
        self.origin.validate(
            "canonical-origin",
            object_key,
            self.origin.content_hash,
            entry.encoded_size(),
        )
    }
}

/// Untrusted hints, requiring current source-catalog and canonical-origin checks.
#[derive(Debug, Default)]
pub struct CapsuleChunkCandidates {
    pub placements: HashMap<[u8; 32], CommittedChunkPlacement>,
    pub anchors: HashMap<[u8; 32], CapsuleChunkAnchor>,
}

#[cfg(any(test, feature = "remote-index"))]
/// Encode one bounded xorb's candidates after full verification and ref publication.
pub(crate) fn capsule_chunk_candidate_rows(
    repo_prefix: &str,
    source_shard_hash: &str,
    xorb_hash: MerkleHash,
    entry: &XorbCatalogEntry,
    origin: &OriginReceipt,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let anchor = CapsuleChunkAnchor {
        source_repo_prefix: repo_prefix.to_owned(),
        source_shard_hash: source_shard_hash.to_owned(),
        body_digest: entry.body_digest().to_owned(),
        origin: origin.clone(),
    };
    anchor.validate_catalog_xorb(&origin.object_key, entry)?;
    if origin.content_hash != <[u8; 32]>::from(xorb_hash) {
        return Err(corrupt("origin names a different xorb"));
    }
    let anchor_id = anchor.id()?;
    let mut rows = vec![(
        capsule_chunk_anchor_key(&anchor_id).to_vec(),
        serde_json::to_vec(&anchor)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?,
    )];
    let mut placements = BTreeMap::new();
    for (index, chunk) in entry.chunks().iter().enumerate() {
        let chunk_hash =
            MerkleHash::from_hex(chunk.hash()).map_err(|_| corrupt("invalid chunk hash"))?;
        placements
            .entry(<[u8; 32]>::from(chunk_hash))
            .or_insert(CommittedChunkPlacement {
                schema_version: RECEIPT_SCHEMA_VERSION,
                chunk_hash: chunk_hash.into(),
                xorb_hash: xorb_hash.into(),
                chunk_index: u32::try_from(index)
                    .map_err(|_| corrupt("chunk position overflows"))?,
                uncompressed_size: chunk.uncompressed_size(),
                origin_proof_id: origin.proof_id(),
                source_anchor_id: anchor_id,
            });
    }
    for (hash, placement) in placements {
        let encoded = placement.encode()?.to_vec();
        // Preserve the exact add-time assertion when another publication replaces
        // the shared head. The placement digest also commits to the chunk key.
        rows.push((
            crate::key_codec::capsule_chunk_placement_key(&placement.placement_id()).to_vec(),
            encoded.clone(),
        ));
        rows.push((capsule_chunk_head_key(&hash).to_vec(), encoded));
    }
    Ok(rows)
}

fn corrupt(reason: &str) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule chunk candidates".to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests;
