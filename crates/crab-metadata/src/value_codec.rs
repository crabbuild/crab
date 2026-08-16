//! Value encodings shared by Crab metadata indexes.
//!
//! These helpers own the byte layouts stored behind metadata keys. Storage
//! adapters decide where values live; this Module decides what the bytes mean.

use crab_xet::xorb::format::{MerkleHash, XorbRef};

use crate::error::{MetadataError, Result};

/// Stored file-index value length: raw 32-byte shard hash.
pub const FILE_INDEX_VALUE_LEN: usize = 32;

/// Versioned committed file-index record length.
pub const COMMITTED_FILE_RECORD_LEN: usize = 105;

/// File-index acceleration record anchored to one committed manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedFileRecord {
    pub recipe_hash: [u8; 32],
    pub shard_hash: MerkleHash,
    pub committed_generation: u64,
    pub shard_index_hash: MerkleHash,
}

/// Encode a generation-pinned committed file-index record.
#[must_use]
pub fn encode_committed_file_record(
    record: &CommittedFileRecord,
) -> [u8; COMMITTED_FILE_RECORD_LEN] {
    let mut bytes = [0u8; COMMITTED_FILE_RECORD_LEN];
    bytes[0] = 2;
    bytes[1..33].copy_from_slice(&record.recipe_hash);
    bytes[33..65].copy_from_slice(&<[u8; 32]>::from(record.shard_hash));
    bytes[65..73].copy_from_slice(&record.committed_generation.to_le_bytes());
    bytes[73..105].copy_from_slice(&<[u8; 32]>::from(record.shard_index_hash));
    bytes
}

/// Decode and validate a generation-pinned committed file-index record.
pub fn decode_committed_file_record(bytes: &[u8]) -> Result<CommittedFileRecord> {
    if bytes.len() != COMMITTED_FILE_RECORD_LEN {
        return Err(corrupt_value(
            "committed file_index value",
            format!(
                "expected {COMMITTED_FILE_RECORD_LEN}-byte record, got {}",
                bytes.len()
            ),
        ));
    }
    if bytes[0] != 2 {
        return Err(corrupt_value(
            "committed file_index value",
            format!("unsupported schema version {}", bytes[0]),
        ));
    }
    let mut recipe_hash = [0u8; 32];
    recipe_hash.copy_from_slice(&bytes[1..33]);
    let mut shard_hash = [0u8; 32];
    shard_hash.copy_from_slice(&bytes[33..65]);
    let mut generation = [0u8; 8];
    generation.copy_from_slice(&bytes[65..73]);
    let mut shard_index_hash = [0u8; 32];
    shard_index_hash.copy_from_slice(&bytes[73..105]);
    let record = CommittedFileRecord {
        recipe_hash,
        shard_hash: MerkleHash::from(shard_hash),
        committed_generation: u64::from_le_bytes(generation),
        shard_index_hash: MerkleHash::from(shard_index_hash),
    };
    if record.recipe_hash == [0; 32]
        || <[u8; 32]>::from(record.shard_hash) == [0; 32]
        || record.committed_generation == 0
        || <[u8; 32]>::from(record.shard_index_hash) == [0; 32]
    {
        return Err(corrupt_value(
            "committed file_index value",
            "record contains an empty committed identity".to_owned(),
        ));
    }
    Ok(record)
}

/// Stored chunk-index value length: 32-byte xorb hash plus two `u32` fields.
pub const CHUNK_INDEX_VALUE_LEN: usize = 40;

/// Stored `sys:*` u32 value length.
pub const U32_SYSTEM_VALUE_LEN: usize = 4;

/// Stored `sys:*` u64 value length.
pub const U64_SYSTEM_VALUE_LEN: usize = 8;

/// Encode a file-index value as a raw shard hash.
#[inline]
#[must_use]
pub fn encode_file_index_value(shard_hash: &MerkleHash) -> [u8; FILE_INDEX_VALUE_LEN] {
    (*shard_hash).into()
}

/// Decode a file-index value from its raw shard-hash bytes.
pub fn decode_file_index_value(bytes: &[u8]) -> Result<MerkleHash> {
    if bytes.len() != FILE_INDEX_VALUE_LEN {
        return Err(corrupt_value(
            "file_index value",
            format!(
                "expected {FILE_INDEX_VALUE_LEN}-byte shard_hash, got {}",
                bytes.len()
            ),
        ));
    }

    let mut hash_bytes = [0u8; FILE_INDEX_VALUE_LEN];
    hash_bytes.copy_from_slice(bytes);
    Ok(MerkleHash::from(hash_bytes))
}

/// Encode a chunk-index value as a compact `XorbRef`.
///
/// Layout: `xorb_hash (32) || chunk_index (u32 LE) || uncompressed_size (u32 LE)`.
#[inline]
#[must_use]
pub fn encode_chunk_index_value(xorb_ref: &XorbRef) -> [u8; CHUNK_INDEX_VALUE_LEN] {
    let mut buf = [0u8; CHUNK_INDEX_VALUE_LEN];
    let hash_bytes: [u8; 32] = xorb_ref.xorb_hash.into();
    buf[..32].copy_from_slice(&hash_bytes);
    buf[32..36].copy_from_slice(&xorb_ref.chunk_index.to_le_bytes());
    buf[36..40].copy_from_slice(&xorb_ref.uncompressed_size.to_le_bytes());
    buf
}

/// Decode a chunk-index value from its compact `XorbRef` bytes.
pub fn decode_chunk_index_value(bytes: &[u8]) -> Result<XorbRef> {
    if bytes.len() != CHUNK_INDEX_VALUE_LEN {
        return Err(corrupt_value(
            "chunk_index value",
            format!(
                "expected {CHUNK_INDEX_VALUE_LEN}-byte xorb_ref, got {}",
                bytes.len()
            ),
        ));
    }

    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(&bytes[..32]);
    let xorb_hash = MerkleHash::from(hash_bytes);

    let mut chunk_index_bytes = [0u8; 4];
    chunk_index_bytes.copy_from_slice(&bytes[32..36]);
    let chunk_index = u32::from_le_bytes(chunk_index_bytes);

    let mut uncompressed_size_bytes = [0u8; 4];
    uncompressed_size_bytes.copy_from_slice(&bytes[36..40]);
    let uncompressed_size = u32::from_le_bytes(uncompressed_size_bytes);

    Ok(XorbRef {
        xorb_hash,
        chunk_index,
        uncompressed_size,
    })
}

/// Encode a u32 `sys:*` value as little-endian bytes.
#[inline]
#[must_use]
pub fn encode_u32_system_value(value: u32) -> [u8; U32_SYSTEM_VALUE_LEN] {
    value.to_le_bytes()
}

/// Decode a u32 `sys:*` value from little-endian bytes.
pub fn decode_u32_system_value(bytes: &[u8]) -> Result<u32> {
    if bytes.len() != U32_SYSTEM_VALUE_LEN {
        return Err(corrupt_value(
            "system u32 value",
            format!("expected 4-byte u32 LE, got {}", bytes.len()),
        ));
    }

    let mut value_bytes = [0u8; U32_SYSTEM_VALUE_LEN];
    value_bytes.copy_from_slice(bytes);
    Ok(u32::from_le_bytes(value_bytes))
}

/// Encode a u64 `sys:*` value as little-endian bytes.
#[inline]
#[must_use]
pub fn encode_u64_system_value(value: u64) -> [u8; U64_SYSTEM_VALUE_LEN] {
    value.to_le_bytes()
}

/// Decode a u64 `sys:*` value from little-endian bytes.
pub fn decode_u64_system_value(bytes: &[u8]) -> Result<u64> {
    if bytes.len() != U64_SYSTEM_VALUE_LEN {
        return Err(corrupt_value(
            "system u64 value",
            format!("expected 8-byte u64 LE, got {}", bytes.len()),
        ));
    }

    let mut value_bytes = [0u8; U64_SYSTEM_VALUE_LEN];
    value_bytes.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(value_bytes))
}

/// Encode the `sys:gc_generation` cursor value.
#[inline]
#[must_use]
pub fn encode_gc_generation_value(value: u64) -> [u8; U64_SYSTEM_VALUE_LEN] {
    encode_u64_system_value(value)
}

/// Decode the `sys:gc_generation` cursor value.
pub fn decode_gc_generation_value(bytes: &[u8]) -> Result<u64> {
    if bytes.len() != U64_SYSTEM_VALUE_LEN {
        return Err(corrupt_value(
            "system gc_generation value",
            format!("expected 8-byte gc_generation, got {}", bytes.len()),
        ));
    }

    decode_u64_system_value(bytes)
}

fn corrupt_value(path: &'static str, reason: String) -> MetadataError {
    MetadataError::CorruptObject {
        path: String::from(path),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed.wrapping_mul(31), seed.wrapping_mul(97), seed])
    }

    fn xorb_ref_for(seed: u64, chunk_index: u32, uncompressed_size: u32) -> XorbRef {
        XorbRef {
            xorb_hash: hash_from_seed(seed),
            chunk_index,
            uncompressed_size,
        }
    }

    #[test]
    fn file_index_value_round_trips_shard_hash() {
        let shard_hash = hash_from_seed(42);
        let encoded = encode_file_index_value(&shard_hash);

        assert_eq!(encoded.len(), FILE_INDEX_VALUE_LEN);
        assert_eq!(
            decode_file_index_value(&encoded).expect("decode file-index value"),
            shard_hash
        );
    }

    #[test]
    fn file_index_value_rejects_wrong_length() {
        let err = decode_file_index_value(&[0u8; 10]).expect_err("short value must fail");
        let msg = format!("{err}");

        assert!(
            msg.contains("32-byte shard_hash"),
            "reason should name the invariant: {msg}"
        );
    }

    #[test]
    fn committed_file_record_round_trips_generation_anchor() {
        let record = CommittedFileRecord {
            recipe_hash: [7; 32],
            shard_hash: hash_from_seed(42),
            committed_generation: 19,
            shard_index_hash: hash_from_seed(43),
        };

        let encoded = encode_committed_file_record(&record);

        assert_eq!(decode_committed_file_record(&encoded).unwrap(), record);
    }

    #[test]
    fn committed_file_record_rejects_legacy_value() {
        let error = decode_committed_file_record(&[0; FILE_INDEX_VALUE_LEN]).unwrap_err();

        assert!(error.to_string().contains("105-byte"));
    }

    #[test]
    fn chunk_index_value_round_trips_xorb_ref() {
        let xorb_ref = xorb_ref_for(99, 7, 4096);
        let encoded = encode_chunk_index_value(&xorb_ref);

        assert_eq!(encoded.len(), CHUNK_INDEX_VALUE_LEN);
        assert_eq!(
            decode_chunk_index_value(&encoded).expect("decode chunk-index value"),
            xorb_ref
        );
    }

    #[test]
    fn chunk_index_value_uses_little_endian_u32_fields() {
        let xorb_ref = xorb_ref_for(11, 0x0102_0304, 0x0506_0708);
        let encoded = encode_chunk_index_value(&xorb_ref);

        assert_eq!(&encoded[32..36], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&encoded[36..40], &[0x08, 0x07, 0x06, 0x05]);
    }

    #[test]
    fn chunk_index_value_rejects_wrong_length() {
        let err = decode_chunk_index_value(&[0u8; 10]).expect_err("short value must fail");
        let msg = format!("{err}");

        assert!(
            msg.contains("40-byte xorb_ref"),
            "reason should name the invariant: {msg}"
        );
    }

    #[test]
    fn u32_system_value_round_trips_little_endian() {
        let encoded = encode_u32_system_value(0x0102_0304);

        assert_eq!(encoded, [0x04, 0x03, 0x02, 0x01]);
        assert_eq!(
            decode_u32_system_value(&encoded).expect("decode u32 sys value"),
            0x0102_0304
        );
    }

    #[test]
    fn u32_system_value_rejects_wrong_length() {
        let err = decode_u32_system_value(&[0u8; 3]).expect_err("short value must fail");
        let msg = format!("{err}");

        assert!(
            msg.contains("expected 4-byte u32 LE"),
            "reason should name the invariant: {msg}"
        );
    }

    #[test]
    fn u64_system_value_round_trips_little_endian() {
        let encoded = encode_u64_system_value(0x0102_0304_0506_0708);

        assert_eq!(encoded, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(
            decode_u64_system_value(&encoded).expect("decode u64 sys value"),
            0x0102_0304_0506_0708
        );
    }

    #[test]
    fn u64_system_value_rejects_wrong_length() {
        let err = decode_u64_system_value(&[0u8; 7]).expect_err("short value must fail");
        let msg = format!("{err}");

        assert!(
            msg.contains("expected 8-byte u64 LE"),
            "reason should name the invariant: {msg}"
        );
    }

    #[test]
    fn gc_generation_value_preserves_error_wording() {
        let encoded = encode_gc_generation_value(42);

        assert_eq!(
            decode_gc_generation_value(&encoded).expect("decode gc generation"),
            42
        );

        let err = decode_gc_generation_value(&[0u8; 7]).expect_err("short value must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("expected 8-byte gc_generation"),
            "reason should name the invariant: {msg}"
        );
    }
}
