//! Bounded xorb metadata and exact provider-identity proofs.

use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;
use crab_xet::hash::{MerkleHash, xorb_hash};
use crab_xet::xorb::builder::{CHUNK_META_ENTRY_SIZE, FOOTER_SIZE, XORB_MAGIC};
use crab_xet::xorb::format::{ChunkMeta, CompressionScheme, MAX_XORB_SIZE, XorbHash};
use object_store::{ObjectMeta, path::Path as ObjectPath};
use tracing::debug;

#[derive(Debug, Clone)]
pub(super) struct RemoteXorbIndex {
    pub(super) hash: XorbHash,
    pub(super) payload_digest: [u8; 32],
    pub(super) chunks: Vec<ChunkMeta>,
}

pub(super) fn corrupt_xorb_index(path: &ObjectPath, reason: impl Into<String>) -> CrabError {
    CrabError::CorruptObject {
        path: path.as_ref().to_owned(),
        reason: reason.into(),
    }
}

pub(super) async fn remote_xorb_index(
    store: &Store,
    path: &ObjectPath,
    object_size: u64,
) -> Result<RemoteXorbIndex> {
    if object_size > MAX_XORB_SIZE as u64 {
        return Err(corrupt_xorb_index(
            path,
            format!("xorb is {object_size} bytes; format limit is {MAX_XORB_SIZE} bytes"),
        ));
    }
    if object_size < FOOTER_SIZE as u64 {
        return Err(corrupt_xorb_index(path, "xorb too small for footer"));
    }

    let footer_start = object_size - FOOTER_SIZE as u64;
    let footer = store.range_get(path, footer_start..object_size).await?;
    if footer.len() != FOOTER_SIZE {
        return Err(corrupt_xorb_index(path, "short xorb footer"));
    }
    if &footer[FOOTER_SIZE - XORB_MAGIC.len()..] != XORB_MAGIC {
        return Err(corrupt_xorb_index(path, "invalid xorb magic"));
    }

    let num_chunks = u32::from_le_bytes(
        footer[0..4]
            .try_into()
            .map_err(|_| corrupt_xorb_index(path, "bad num_chunks bytes"))?,
    );
    let meta_offset = u64::from_le_bytes(
        footer[4..12]
            .try_into()
            .map_err(|_| corrupt_xorb_index(path, "bad meta_offset bytes"))?,
    );
    let payload_digest = footer[12..44]
        .try_into()
        .map_err(|_| corrupt_xorb_index(path, "bad payload digest bytes"))?;
    let metadata_size = u64::from(num_chunks)
        .checked_mul(CHUNK_META_ENTRY_SIZE as u64)
        .ok_or_else(|| corrupt_xorb_index(path, "metadata region size overflow"))?;
    let metadata_end = meta_offset
        .checked_add(metadata_size)
        .ok_or_else(|| corrupt_xorb_index(path, "metadata region end overflow"))?;
    let expected_size = metadata_end
        .checked_add(FOOTER_SIZE as u64)
        .ok_or_else(|| corrupt_xorb_index(path, "xorb size overflow"))?;
    if expected_size != object_size {
        return Err(corrupt_xorb_index(path, "metadata region size mismatch"));
    }

    let metadata_len = usize::try_from(metadata_size)
        .map_err(|_| corrupt_xorb_index(path, "metadata region too large"))?;
    let metadata = store.range_get(path, meta_offset..metadata_end).await?;
    if metadata.len() != metadata_len {
        return Err(corrupt_xorb_index(path, "short xorb metadata region"));
    }

    let mut cursor = 0usize;
    let mut expected_payload_offset = 0u64;
    let mut hash_pairs = Vec::with_capacity(num_chunks as usize);
    let mut chunks = Vec::with_capacity(num_chunks as usize);
    for _ in 0..num_chunks {
        let hash_bytes: [u8; 32] = metadata[cursor..cursor + 32]
            .try_into()
            .map_err(|_| corrupt_xorb_index(path, "bad chunk hash bytes"))?;
        let hash = MerkleHash::from(hash_bytes);
        cursor += 32;

        let offset = u32::from_le_bytes(
            metadata[cursor..cursor + 4]
                .try_into()
                .map_err(|_| corrupt_xorb_index(path, "bad chunk offset"))?,
        );
        cursor += 4;

        let compressed_len = u32::from_le_bytes(
            metadata[cursor..cursor + 4]
                .try_into()
                .map_err(|_| corrupt_xorb_index(path, "bad compressed_len"))?,
        );
        cursor += 4;

        let uncompressed_len = u32::from_le_bytes(
            metadata[cursor..cursor + 4]
                .try_into()
                .map_err(|_| corrupt_xorb_index(path, "bad uncompressed_len"))?,
        );
        cursor += 4;

        let scheme_byte = metadata[cursor];
        let scheme = CompressionScheme::try_from(scheme_byte)
            .map_err(|_| corrupt_xorb_index(path, "invalid compression scheme byte"))?;
        cursor += 1;

        let chunk_end = u64::from(offset).saturating_add(u64::from(compressed_len));
        if u64::from(offset) != expected_payload_offset || chunk_end > meta_offset {
            return Err(corrupt_xorb_index(
                path,
                "chunk payload ranges are not contiguous before metadata",
            ));
        }
        expected_payload_offset = chunk_end;
        hash_pairs.push((hash, u64::from(uncompressed_len)));
        chunks.push(ChunkMeta {
            hash,
            offset,
            compressed_len,
            uncompressed_len,
            scheme,
        });
    }
    if expected_payload_offset != meta_offset {
        return Err(corrupt_xorb_index(
            path,
            "chunk payload ranges do not cover the serialized payload region",
        ));
    }

    Ok(RemoteXorbIndex {
        hash: xorb_hash(&hash_pairs),
        payload_digest,
        chunks,
    })
}

pub(super) fn committed_origin_receipt_matches_remote(
    receipt: &crab_metadata::receipts::OriginReceipt,
    xorb_hash: &XorbHash,
    path: &ObjectPath,
    index: &RemoteXorbIndex,
    meta: &ObjectMeta,
) -> bool {
    if receipt
        .validate(
            "canonical-origin",
            path.as_ref(),
            (*xorb_hash).into(),
            meta.size,
        )
        .is_err()
        || receipt.payload_digest != index.payload_digest
        || index.hash != *xorb_hash
        || meta.location != *path
    {
        return false;
    }

    let has_identity = receipt.etag.is_some() || receipt.object_version.is_some();
    has_identity
        && receipt
            .etag
            .as_ref()
            .is_none_or(|etag| meta.e_tag.as_ref() == Some(etag))
        && receipt
            .object_version
            .as_ref()
            .is_none_or(|version| meta.version.as_ref() == Some(version))
}

pub(super) fn cached_remote_xorb_index_matches(
    cache: &crate::cache::LocalCache,
    xorb_hash: &XorbHash,
    meta: &ObjectMeta,
) -> Option<RemoteXorbIndex> {
    match cache.cached_remote_xorb_index(
        xorb_hash,
        meta.size,
        meta.e_tag.as_deref(),
        meta.version.as_deref(),
    ) {
        Ok(Some(index)) => Some(RemoteXorbIndex {
            hash: index.xorb_hash,
            payload_digest: index.payload_digest,
            chunks: index.chunks,
        }),
        Ok(None) => None,
        Err(e) => {
            debug!(
                xorb_hash = %xorb_hash.hex(),
                error = %e,
                "remote xorb index cache lookup failed"
            );
            None
        }
    }
}

pub(super) fn record_remote_xorb_index(
    cache: &crate::cache::LocalCache,
    xorb_hash: &XorbHash,
    meta: &ObjectMeta,
    index: &RemoteXorbIndex,
) {
    match cache.record_remote_xorb_index(
        xorb_hash,
        &index.payload_digest,
        meta.size,
        meta.e_tag.as_deref(),
        meta.version.as_deref(),
        &index.chunks,
    ) {
        Ok(_) => {}
        Err(e) => debug!(
            xorb_hash = %xorb_hash.hex(),
            error = %e,
            "remote xorb index cache update failed"
        ),
    }
}

pub(super) fn record_remote_xorb_payload_proof(
    cache: &crate::cache::LocalCache,
    xorb_hash: &XorbHash,
    index: &RemoteXorbIndex,
    meta: &ObjectMeta,
) {
    match cache.record_remote_xorb_proof(
        xorb_hash,
        &index.payload_digest,
        meta.size,
        meta.e_tag.as_deref(),
        meta.version.as_deref(),
    ) {
        Ok(_) => {}
        Err(e) => debug!(
            xorb_hash = %xorb_hash.hex(),
            error = %e,
            "remote xorb proof cache update failed"
        ),
    }
}
