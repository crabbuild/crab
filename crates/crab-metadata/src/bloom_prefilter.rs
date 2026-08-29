//! Shared bloom pre-filter for shard fetches.
//!
//! When a canonical v1 shard has a bloom trailer, a small Range GET on the tail
//! (≤ 4 KiB) can prove whether a file or chunk hash is definitely absent
//! before the full shard body is pulled over the wire. Both the push-path
//! shard synchronizer and the hydrate-path shard resolution use this to
//! cut unnecessary shard downloads.
//!
//! The trailer layout is identical to the one written by Crab's shard writer:
//!
//! ```text
//!   ... xet shard body ...
//!   [bloom section: variable, encoded by ShardBloom::encode]
//!   [bloom_offset: u64 LE]     ← start of bloom section
//!   [magic: "SH01" (4 bytes)]  ← last 4 bytes of object
//! ```
//!
//! The pre-filter is strictly advisory: on any I/O or parse failure we
//! return [`BloomCheck::NoBloom`] (or an error the caller falls back on)
//! so a transient network blip never masks a real shard.

use object_store::path::Path;

use crate::error::{MetadataError, Result};
use crab_storage::Store;
use crab_xet::shard_bloom::ShardBloom;
use crab_xet::shard_parse::MAX_SHARD_SIZE_BYTES;
use crab_xet::xorb::format::MerkleHash;

/// Size of the canonical v1 bloom trailer.
pub(crate) const SHARD_V1_TRAILER_SIZE: u64 = 12;

/// Magic bytes at the end of a canonical v1 shard bloom trailer.
pub(crate) const SHARD_V1_MAGIC: &[u8; 4] = b"SH01";

/// Maximum size of the bloom section we're willing to pull via Range GET.
///
/// The design bounds the bloom pre-filter at ≤ 4 KiB so a small Range
/// GET stays within a single round-trip's worth of TCP window. When a
/// shard's bloom section exceeds this budget (very large file sets), we
/// fall back to a full shard download — the pre-filter's goal is latency
/// reduction, not correctness, so skipping it is always safe.
const MAX_BLOOM_RANGE_BYTES: u64 = 4 * 1024;

/// Outcome of a bloom pre-filter check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BloomCheck {
    /// The bloom proves the queried hash is not in this shard. Callers
    /// should treat this as a negative answer: for the push path, the
    /// shard can be skipped; for the hydrate path, the file-index entry
    /// is stale or the shard is the wrong shard for this file.
    DefinitelyAbsent,
    /// The bloom reports a possible hit. The caller must download the
    /// full shard to determine presence authoritatively.
    PossiblyPresent,
    /// No bloom is available or usable: the v1 shard has no trailer, the trailer
    /// is too small to parse, or the bloom section exceeds the 4 KiB
    /// Range GET budget. Callers should proceed with a full download.
    NoBloom,
}

/// Check a shard's bloom against a single file hash.
///
/// Returns [`BloomCheck::DefinitelyAbsent`] only when the shard carries
/// a canonical v1 bloom and the bloom definitively excludes `file_hash`. Every
/// other outcome (plain shard, oversized bloom, unreadable trailer, bloom
/// says "maybe") resolves to [`BloomCheck::PossiblyPresent`] or
/// [`BloomCheck::NoBloom`] so the caller falls back to a full shard
/// download.
///
/// # Errors
///
/// Propagates I/O errors from [`Store::head`] and [`Store::range_get`]
/// so the caller can decide whether to retry or fall back. Parse errors
/// on an apparently-present trailer are surfaced as
/// [`MetadataError::CorruptObject`].
pub async fn check_shard_file_bloom(
    store: &Store,
    shard_path: &Path,
    file_hash: &MerkleHash,
) -> Result<BloomCheck> {
    let bloom = match load_bloom_if_any(store, shard_path).await? {
        Some(b) => b,
        None => return Ok(BloomCheck::NoBloom),
    };
    if bloom.maybe_contains_file(file_hash) {
        Ok(BloomCheck::PossiblyPresent)
    } else {
        Ok(BloomCheck::DefinitelyAbsent)
    }
}

/// Check a shard's bloom against a set of chunk hashes.
///
/// Returns [`BloomCheck::DefinitelyAbsent`] only when none of the chunk
/// hashes are reported as possibly present. If any chunk hash might be
/// in the shard, returns [`BloomCheck::PossiblyPresent`].
///
/// # Errors
///
/// Same semantics as [`check_shard_file_bloom`].
pub async fn check_shard_chunk_bloom(
    store: &Store,
    shard_path: &Path,
    chunk_hashes: &std::collections::HashSet<MerkleHash>,
) -> Result<BloomCheck> {
    let bloom = match load_bloom_if_any(store, shard_path).await? {
        Some(b) => b,
        None => return Ok(BloomCheck::NoBloom),
    };
    for hash in chunk_hashes {
        if bloom.maybe_contains_chunk(hash) {
            return Ok(BloomCheck::PossiblyPresent);
        }
    }
    Ok(BloomCheck::DefinitelyAbsent)
}

/// Fetch and decode a shard's bloom section when one exists and fits in
/// the Range GET budget. Returns `Ok(None)` for v1 shards without a bloom,
/// shards too small for a trailer, and shards whose bloom exceeds
/// [`MAX_BLOOM_RANGE_BYTES`].
async fn load_bloom_if_any(store: &Store, shard_path: &Path) -> Result<Option<ShardBloom>> {
    let meta = store.head(shard_path).await?;
    let shard_size = meta.size;

    if shard_size > MAX_SHARD_SIZE_BYTES as u64 {
        return Err(MetadataError::CorruptObject {
            path: shard_path.to_string(),
            reason: format!(
                "shard is {shard_size} bytes; format limit is {MAX_SHARD_SIZE_BYTES} bytes"
            ),
        });
    }

    if shard_size < SHARD_V1_TRAILER_SIZE {
        return Ok(None);
    }

    let trailer_start = shard_size - SHARD_V1_TRAILER_SIZE;
    let trailer = store
        .range_get(shard_path, trailer_start..shard_size)
        .await?;

    if trailer.len() < SHARD_V1_TRAILER_SIZE as usize {
        return Ok(None);
    }

    if &trailer[8..12] != SHARD_V1_MAGIC {
        // Canonical v1 shard without a bloom.
        return Ok(None);
    }

    let bloom_offset =
        u64::from_le_bytes(
            trailer[0..8]
                .try_into()
                .map_err(|_| MetadataError::CorruptObject {
                    path: shard_path.to_string(),
                    reason: "bad bloom_offset bytes in trailer".to_owned(),
                })?,
        );

    if bloom_offset >= trailer_start {
        return Err(MetadataError::CorruptObject {
            path: shard_path.to_string(),
            reason: "bloom_offset past bloom data".to_owned(),
        });
    }

    let bloom_bytes_len = trailer_start - bloom_offset;
    if bloom_bytes_len > MAX_BLOOM_RANGE_BYTES {
        // Bloom section is larger than the Range GET budget; fall back
        // to a full shard download. This keeps the pre-filter a cheap
        // tail-read rather than a partial body fetch.
        tracing::debug!(
            shard = %shard_path,
            bloom_bytes = bloom_bytes_len,
            cap = MAX_BLOOM_RANGE_BYTES,
            "shard bloom exceeds range-get budget, skipping pre-filter",
        );
        return Ok(None);
    }

    let bloom_bytes = store
        .range_get(shard_path, bloom_offset..trailer_start)
        .await?;
    let bloom = ShardBloom::decode(&bloom_bytes)?;
    Ok(Some(bloom))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;

    use super::*;
    use crab_xet::shard_bloom::ShardBloom;

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([
            seed,
            seed.wrapping_mul(31),
            seed.wrapping_mul(97),
            seed.wrapping_mul(127),
        ])
    }

    fn memory_store() -> Store {
        Store::new(Arc::new(InMemory::new()))
    }

    /// Build a synthetic shard body: a deterministic payload followed by a
    /// canonical v1 bloom trailer constructed from the provided hashes.
    fn make_bloom_shard(file_hashes: &[MerkleHash], chunk_hashes: &[MerkleHash]) -> Bytes {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"synthetic-shard-body-v1");
        let bloom_offset = buf.len() as u64;
        let bloom = ShardBloom::build(file_hashes, chunk_hashes);
        buf.extend_from_slice(&bloom.encode());
        buf.extend_from_slice(&bloom_offset.to_le_bytes());
        buf.extend_from_slice(SHARD_V1_MAGIC);
        Bytes::from(buf)
    }

    /// Build a synthetic v1 shard (no bloom trailer).
    fn make_v1_shard() -> Bytes {
        Bytes::from_static(b"synthetic-v1-shard-no-bloom-here")
    }

    #[tokio::test]
    async fn file_bloom_detects_absent_hash() {
        let store = memory_store();
        let path = Path::from("shards/test-absent");
        let present: Vec<MerkleHash> = (1..=20).map(hash_from_seed).collect();
        let shard = make_bloom_shard(&present, &[]);
        store.put(&path, shard).await.unwrap();

        // A hash that was never inserted — bloom should report absent.
        let absent = hash_from_seed(9999);
        let result = check_shard_file_bloom(&store, &path, &absent)
            .await
            .unwrap();
        assert_eq!(result, BloomCheck::DefinitelyAbsent);
    }

    #[tokio::test]
    async fn file_bloom_reports_present_for_inserted_hash() {
        let store = memory_store();
        let path = Path::from("shards/test-present");
        let present: Vec<MerkleHash> = (1..=20).map(hash_from_seed).collect();
        let shard = make_bloom_shard(&present, &[]);
        store.put(&path, shard).await.unwrap();

        let result = check_shard_file_bloom(&store, &path, &present[7])
            .await
            .unwrap();
        // No false negatives: must be PossiblyPresent.
        assert_eq!(result, BloomCheck::PossiblyPresent);
    }

    #[tokio::test]
    async fn v1_shard_returns_no_bloom() {
        let store = memory_store();
        let path = Path::from("shards/test-v1");
        store.put(&path, make_v1_shard()).await.unwrap();

        let result = check_shard_file_bloom(&store, &path, &hash_from_seed(1))
            .await
            .unwrap();
        assert_eq!(result, BloomCheck::NoBloom);
    }

    #[tokio::test]
    async fn chunk_bloom_detects_absent_set() {
        let store = memory_store();
        let path = Path::from("shards/chunks-absent");
        let chunk_hashes: Vec<MerkleHash> = (100..=200).map(hash_from_seed).collect();
        let shard = make_bloom_shard(&[], &chunk_hashes);
        store.put(&path, shard).await.unwrap();

        let query: std::collections::HashSet<MerkleHash> =
            (9000..=9010).map(hash_from_seed).collect();
        let result = check_shard_chunk_bloom(&store, &path, &query)
            .await
            .unwrap();
        assert_eq!(result, BloomCheck::DefinitelyAbsent);
    }

    #[tokio::test]
    async fn chunk_bloom_reports_present_when_any_match() {
        let store = memory_store();
        let path = Path::from("shards/chunks-present");
        let chunk_hashes: Vec<MerkleHash> = (100..=200).map(hash_from_seed).collect();
        let shard = make_bloom_shard(&[], &chunk_hashes);
        store.put(&path, shard).await.unwrap();

        let mut query: std::collections::HashSet<MerkleHash> =
            (9000..=9010).map(hash_from_seed).collect();
        query.insert(chunk_hashes[50]);

        let result = check_shard_chunk_bloom(&store, &path, &query)
            .await
            .unwrap();
        assert_eq!(result, BloomCheck::PossiblyPresent);
    }

    #[tokio::test]
    async fn oversized_bloom_falls_back_to_no_bloom() {
        // Build a bloom with more than MAX_BLOOM_RANGE_BYTES of data.
        // At 10 bits per element, we need > 4096 bytes = 32768 bits =>
        // > ~3277 elements. Use 5000 to be safely over the threshold.
        let store = memory_store();
        let path = Path::from("shards/test-oversized");
        let many: Vec<MerkleHash> = (0..5000).map(hash_from_seed).collect();
        let shard = make_bloom_shard(&many, &[]);
        store.put(&path, shard).await.unwrap();

        let result = check_shard_file_bloom(&store, &path, &hash_from_seed(999_999))
            .await
            .unwrap();
        // Bloom section is too large — we bail out and let the caller
        // download the full shard.
        assert_eq!(result, BloomCheck::NoBloom);
    }

    #[tokio::test]
    async fn shard_smaller_than_trailer_returns_no_bloom() {
        let store = memory_store();
        let path = Path::from("shards/tiny");
        store.put(&path, Bytes::from_static(b"xx")).await.unwrap();

        let result = check_shard_file_bloom(&store, &path, &hash_from_seed(1))
            .await
            .unwrap();
        assert_eq!(result, BloomCheck::NoBloom);
    }
}
