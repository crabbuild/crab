//! Keyspace conventions shared by both SlateDB metadata databases.
//!
//! Every key starts with a single sentinel byte that discriminates the
//! content-addressed and system keyspaces. Mirrors the ZeroFS key-codec
//! pattern: one place defines the prefix bytes, every store imports it.
//!
//! Layout:
//!
//! - `0x01 || hash[32]` — 33-byte content key (file_hash or chunk_hash).
//! - `0x02 || hash[32] || identity` — immutable committed history.
//! - `0x03 || hash[32]` — latest committed chunk-receipt candidate.
//! - `0x04 || proof_id[32]` — immutable xorb origin proof.
//! - `0x05 || anchor_id[32]` — immutable source-shard anchor.
//! - `0xFF || b"sys:" || name` — system key (format_version, epoch,
//!   created_at, gc_generation).
//!
//! The prefix byte is worth the one extra byte per key: SSTable
//! compression eats it, and future diagnostic tooling can classify any
//! key at a glance.

use crab_xet::xorb::format::MerkleHash;

use crate::error::{MetadataError, Result};

/// Prefix byte for content-addressed keys (file_hash or chunk_hash).
pub const PREFIX_CONTENT: u8 = 0x01;

/// Prefix byte for generation- or receipt-pinned committed records.
/// Legacy unversioned content keys remain candidates during migration and
/// are never read through this namespace.
pub const PREFIX_COMMITTED: u8 = 0x02;

/// Prefix byte for the point-readable latest committed chunk receipt.
///
/// This is rebuildable acceleration over the immutable receipt history under
/// [`PREFIX_COMMITTED`]. Consumers still validate generation, GC roots, shard
/// membership, and canonical-origin proof before accepting the candidate.
pub const PREFIX_COMMITTED_HEAD: u8 = 0x03;

/// Prefix byte for immutable xorb origin proofs.
pub const PREFIX_ORIGIN_PROOF: u8 = 0x04;

/// Prefix byte for immutable repository-shard source anchors.
pub const PREFIX_SOURCE_ANCHOR: u8 = 0x05;

/// Prefix byte for the `sys:*` system-key namespace. Chosen as `0xFF`
/// so it sorts after every content key and is trivial to filter out of
/// content-only scans.
pub const PREFIX_SYSTEM: u8 = 0xFF;

/// Total byte length of an encoded content key: prefix + 32-byte hash.
pub const CONTENT_KEY_LEN: usize = 33;

/// Prefix plus content hash for generation-keyed file records.
pub const COMMITTED_CONTENT_PREFIX_LEN: usize = 33;

/// `PREFIX_COMMITTED || hash[32] || generation_be[8]`.
pub const COMMITTED_FILE_KEY_LEN: usize = COMMITTED_CONTENT_PREFIX_LEN + 8;

/// `PREFIX_COMMITTED || hash[32] || receipt_id[32]`.
pub const COMMITTED_CHUNK_KEY_LEN: usize = COMMITTED_CONTENT_PREFIX_LEN + 32;

/// `PREFIX_COMMITTED_HEAD || hash[32]`.
pub const COMMITTED_CHUNK_HEAD_KEY_LEN: usize = COMMITTED_CONTENT_PREFIX_LEN;

/// Prefix plus a 32-byte proof or anchor identifier.
pub const PROOF_RECORD_KEY_LEN: usize = 33;

/// Name of the GC generation system key (stored as u64 LE).
pub const SYS_GC_GENERATION: &str = "gc_generation";

/// Name of the write-batch epoch system key (stored as u64 LE).
pub const SYS_EPOCH: &str = "epoch";

/// Name of the creation timestamp system key (unix ms, u64 LE).
pub const SYS_CREATED_AT: &str = "created_at";

/// Name of the schema-version system key (u32 LE).
pub const SYS_FORMAT_VERSION: &str = "format_version";

/// Encode a content hash as a 33-byte SlateDB key.
///
/// Layout: `0x01 || hash[32]`. Matches the ZeroFS convention so every
/// stored content-addressed value lives in one contiguous prefix.
#[inline]
pub fn encode_content_key(hash: &MerkleHash) -> [u8; CONTENT_KEY_LEN] {
    let mut key = [0u8; CONTENT_KEY_LEN];
    key[0] = PREFIX_CONTENT;
    let bytes: [u8; 32] = (*hash).into();
    key[1..].copy_from_slice(&bytes);
    key
}

/// Build the ordered prefix shared by all committed records for one hash.
#[inline]
pub fn encode_committed_content_prefix(hash: &MerkleHash) -> [u8; COMMITTED_CONTENT_PREFIX_LEN] {
    let mut key = [0u8; COMMITTED_CONTENT_PREFIX_LEN];
    key[0] = PREFIX_COMMITTED;
    let bytes: [u8; 32] = (*hash).into();
    key[1..].copy_from_slice(&bytes);
    key
}

/// Encode one generation-pinned file-index key.
#[inline]
pub fn encode_committed_file_key(
    hash: &MerkleHash,
    generation: u64,
) -> [u8; COMMITTED_FILE_KEY_LEN] {
    let mut key = [0u8; COMMITTED_FILE_KEY_LEN];
    key[..COMMITTED_CONTENT_PREFIX_LEN].copy_from_slice(&encode_committed_content_prefix(hash));
    key[COMMITTED_CONTENT_PREFIX_LEN..].copy_from_slice(&generation.to_be_bytes());
    key
}

/// Decode and cross-check a generation-pinned file-index key.
pub fn decode_committed_file_key(key: &[u8]) -> Result<(MerkleHash, u64)> {
    if key.len() != COMMITTED_FILE_KEY_LEN || key[0] != PREFIX_COMMITTED {
        return Err(MetadataError::CorruptObject {
            path: format!("metadb key {}", hex_encode(key)),
            reason: format!("expected {COMMITTED_FILE_KEY_LEN}-byte committed file key"),
        });
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&key[1..COMMITTED_CONTENT_PREFIX_LEN]);
    let mut generation = [0u8; 8];
    generation.copy_from_slice(&key[COMMITTED_CONTENT_PREFIX_LEN..]);
    Ok((MerkleHash::from(hash), u64::from_be_bytes(generation)))
}

/// Encode one receipt-pinned committed chunk-index key.
#[inline]
pub fn encode_committed_chunk_key(
    hash: &MerkleHash,
    receipt_id: &[u8; 32],
) -> [u8; COMMITTED_CHUNK_KEY_LEN] {
    let mut key = [0u8; COMMITTED_CHUNK_KEY_LEN];
    key[..COMMITTED_CONTENT_PREFIX_LEN].copy_from_slice(&encode_committed_content_prefix(hash));
    key[COMMITTED_CONTENT_PREFIX_LEN..].copy_from_slice(receipt_id);
    key
}

/// Encode the point-readable latest-receipt candidate key for one chunk.
#[inline]
pub fn encode_committed_chunk_head_key(hash: &MerkleHash) -> [u8; COMMITTED_CHUNK_HEAD_KEY_LEN] {
    let mut key = [0u8; COMMITTED_CHUNK_HEAD_KEY_LEN];
    key[0] = PREFIX_COMMITTED_HEAD;
    let bytes: [u8; 32] = (*hash).into();
    key[1..].copy_from_slice(&bytes);
    key
}

/// Encode an immutable xorb-origin proof key.
#[inline]
pub fn encode_origin_proof_key(proof_id: &[u8; 32]) -> [u8; PROOF_RECORD_KEY_LEN] {
    encode_proof_record_key(PREFIX_ORIGIN_PROOF, proof_id)
}

/// Encode an immutable source-anchor key.
#[inline]
pub fn encode_source_anchor_key(anchor_id: &[u8; 32]) -> [u8; PROOF_RECORD_KEY_LEN] {
    encode_proof_record_key(PREFIX_SOURCE_ANCHOR, anchor_id)
}

fn encode_proof_record_key(prefix: u8, id: &[u8; 32]) -> [u8; PROOF_RECORD_KEY_LEN] {
    let mut key = [0u8; PROOF_RECORD_KEY_LEN];
    key[0] = prefix;
    key[1..].copy_from_slice(id);
    key
}

/// Decode and cross-check a latest-receipt candidate key.
pub fn decode_committed_chunk_head_key(key: &[u8]) -> Result<MerkleHash> {
    if key.len() != COMMITTED_CHUNK_HEAD_KEY_LEN || key[0] != PREFIX_COMMITTED_HEAD {
        return Err(MetadataError::CorruptObject {
            path: format!("metadb key {}", hex_encode(key)),
            reason: format!(
                "expected {COMMITTED_CHUNK_HEAD_KEY_LEN}-byte committed chunk head key"
            ),
        });
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&key[1..]);
    Ok(MerkleHash::from(hash))
}

/// Decode and cross-check a receipt-pinned chunk-index key.
pub fn decode_committed_chunk_key(key: &[u8]) -> Result<(MerkleHash, [u8; 32])> {
    if key.len() != COMMITTED_CHUNK_KEY_LEN || key[0] != PREFIX_COMMITTED {
        return Err(MetadataError::CorruptObject {
            path: format!("metadb key {}", hex_encode(key)),
            reason: format!("expected {COMMITTED_CHUNK_KEY_LEN}-byte committed chunk key"),
        });
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&key[1..COMMITTED_CONTENT_PREFIX_LEN]);
    let mut receipt_id = [0u8; 32];
    receipt_id.copy_from_slice(&key[COMMITTED_CONTENT_PREFIX_LEN..]);
    Ok((MerkleHash::from(hash), receipt_id))
}

/// Decode a SlateDB key as a content hash.
///
/// Rejects keys whose leading byte is not `PREFIX_CONTENT` or whose
/// total length is not 33 bytes. The caller is responsible for deciding
/// which logical database the key came from; this function only
/// enforces the keyspace invariant.
pub fn decode_content_key(key: &[u8]) -> Result<MerkleHash> {
    if key.len() != CONTENT_KEY_LEN {
        return Err(MetadataError::CorruptObject {
            path: format!("metadb key {}", hex_encode(key)),
            reason: format!(
                "expected {CONTENT_KEY_LEN}-byte content key, got {}",
                key.len()
            ),
        });
    }
    if key[0] != PREFIX_CONTENT {
        return Err(MetadataError::CorruptObject {
            path: format!("metadb key {}", hex_encode(key)),
            reason: format!(
                "expected content-key prefix {PREFIX_CONTENT:#04x}, got {:#04x}",
                key[0]
            ),
        });
    }
    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(&key[1..]);
    Ok(MerkleHash::from(hash_bytes))
}

/// Build the byte sequence for a system key.
///
/// Result layout: `0xFF || b"sys:" || name`. Returned as `Vec<u8>`
/// because names vary in length and callers typically hand it straight
/// to a SlateDB `put` / `get` which accepts any `AsRef<[u8]>`.
pub fn encode_system_key(name: &str) -> Vec<u8> {
    // Pre-size: prefix byte + "sys:" literal + name length. Saves the
    // grow allocations for the common short names.
    let mut key = Vec::with_capacity(1 + 4 + name.len());
    key.push(PREFIX_SYSTEM);
    key.extend_from_slice(b"sys:");
    key.extend_from_slice(name.as_bytes());
    key
}

/// Lowercase hex helper for corruption-error payloads. Kept inline
/// rather than adding a `hex` dependency for this single call site.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        // `write!` into a `String` is infallible for `{:02x}`, so the
        // `Result` is intentionally discarded.
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed.wrapping_mul(31), seed.wrapping_mul(97), seed])
    }

    #[test]
    fn encode_content_key_starts_with_prefix_byte() {
        let hash = hash_from_seed(1);
        let encoded = encode_content_key(&hash);
        assert_eq!(encoded.len(), CONTENT_KEY_LEN);
        assert_eq!(encoded[0], PREFIX_CONTENT);
    }

    #[test]
    fn encode_decode_content_key_round_trip() {
        for seed in [0u64, 1, 42, u64::MAX] {
            let hash = hash_from_seed(seed);
            let encoded = encode_content_key(&hash);
            let decoded = decode_content_key(&encoded).expect("round-trip decode");
            assert_eq!(decoded, hash, "round-trip mismatch at seed {seed}");
        }
    }

    #[test]
    fn decode_content_key_rejects_wrong_length() {
        let err = decode_content_key(&[PREFIX_CONTENT; 10]).expect_err("short key should fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("33-byte"),
            "reason should name the invariant: {msg}"
        );
    }

    #[test]
    fn decode_content_key_rejects_wrong_prefix() {
        let mut bad = [0u8; CONTENT_KEY_LEN];
        bad[0] = PREFIX_SYSTEM;
        let err = decode_content_key(&bad).expect_err("system prefix must not decode as content");
        let msg = format!("{err}");
        assert!(
            msg.contains("content-key prefix"),
            "reason should identify prefix mismatch: {msg}"
        );
    }

    #[test]
    fn committed_chunk_head_key_round_trips_without_colliding_with_history() {
        let hash = hash_from_seed(41);
        let head = encode_committed_chunk_head_key(&hash);
        let history = encode_committed_chunk_key(&hash, &[7; 32]);

        assert_eq!(decode_committed_chunk_head_key(&head).unwrap(), hash);
        assert_eq!(head[0], PREFIX_COMMITTED_HEAD);
        assert_eq!(history[0], PREFIX_COMMITTED);
        assert_ne!(head.as_slice(), history.as_slice());
    }

    #[test]
    fn encode_system_key_starts_with_system_prefix_and_sys_literal() {
        let key = encode_system_key(SYS_GC_GENERATION);
        assert_eq!(key[0], PREFIX_SYSTEM);
        assert_eq!(&key[1..5], b"sys:");
        assert_eq!(&key[5..], SYS_GC_GENERATION.as_bytes());
    }

    #[test]
    fn encode_system_key_never_collides_with_content_length() {
        // A content key is always exactly 33 bytes starting with 0x01.
        // A system key can be any length but starts with 0xFF; even if
        // a system name were chosen such that the total length hit 33,
        // the leading byte differs — no collision is possible.
        let sys_33 = encode_system_key(&"x".repeat(28)); // 1 + 4 + 28 = 33
        assert_eq!(sys_33.len(), CONTENT_KEY_LEN);
        assert_ne!(sys_33[0], PREFIX_CONTENT);
    }

    #[test]
    fn encode_system_key_covers_all_named_constants() {
        // Smoke-test that every reserved `sys:*` name encodes cleanly.
        for name in [
            SYS_GC_GENERATION,
            SYS_EPOCH,
            SYS_CREATED_AT,
            SYS_FORMAT_VERSION,
        ] {
            let key = encode_system_key(name);
            assert!(key.len() > 5);
            assert_eq!(key[0], PREFIX_SYSTEM);
        }
    }
}
