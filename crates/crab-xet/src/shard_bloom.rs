//! Shard bloom filter for fast membership pre-checks on file and chunk hashes.
//!
//! Each canonical v1 shard can carry an optional bloom footer containing
//! two sub-blooms — one for file hashes and one for chunk hashes — both
//! targeting a 1% false-positive rate. The bloom is appended after the
//! standard shard data and referenced by a `bloom_offset` in the extended
//! footer.
//!
//! Encoding format:
//! ```text
//! [magic: 4 bytes "BLOM"]
//! [version: 1 byte (1)]
//! [file_bloom_num_bits: 8 bytes LE]
//! [file_bloom_num_hashes: 4 bytes LE]
//! [file_bloom_data_len: 4 bytes LE]
//! [file_bloom_data: variable]
//! [chunk_bloom_num_bits: 8 bytes LE]
//! [chunk_bloom_num_hashes: 4 bytes LE]
//! [chunk_bloom_data_len: 4 bytes LE]
//! [chunk_bloom_data: variable]
//! ```

use xet_core_structures::merklehash::MerkleHash;

use crate::error::{Result, XetError};

/// Magic bytes identifying a shard bloom section.
const BLOOM_MAGIC: &[u8; 4] = b"BLOM";

/// Current bloom encoding version.
const BLOOM_VERSION: u8 = 1;

/// Number of hash functions for ≈1% FP rate (`ceil(num_bits/n * ln2)` ≈ 7).
const TARGET_NUM_HASHES: u32 = 7;

/// Bits-per-element multiplier for ≈1% FP rate.
/// Derived from: `ceil(-ln(0.01) / ln(2)^2)` ≈ 9.585, we use 10 for
/// simplicity and slightly better FP rate.
const BITS_PER_ELEMENT: u64 = 10;

/// Minimum bloom size in bits to avoid degenerate filters.
const MIN_BLOOM_BITS: u64 = 64;

/// Shard bloom filter containing two sub-blooms for file hashes and chunk hashes.
pub struct ShardBloom {
    file_bloom: BloomFilter,
    chunk_bloom: BloomFilter,
}

/// Simple bloom filter implementation targeting 1% false positive rate.
///
/// Uses the double-hashing technique: `h_i(x) = h1(x) + i * h2(x)` where
/// h1 and h2 are extracted from the `MerkleHash`'s u64 components (which
/// are already well-distributed blake3 output).
struct BloomFilter {
    bits: Vec<u8>,
    num_hashes: u32,
    num_bits: u64,
}

impl BloomFilter {
    /// Create a new bloom filter sized for `n` elements at ≈1% FP rate.
    fn new(n: usize) -> Self {
        let num_bits = if n == 0 {
            MIN_BLOOM_BITS
        } else {
            (n as u64 * BITS_PER_ELEMENT).max(MIN_BLOOM_BITS)
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bloom size is bounded by element count × 10 bits; realistic inputs stay well under usize::MAX"
        )]
        let byte_len = num_bits.div_ceil(8) as usize;
        Self {
            bits: vec![0u8; byte_len],
            num_hashes: TARGET_NUM_HASHES,
            num_bits,
        }
    }

    /// Reconstruct from pre-existing data (used by `decode`).
    fn from_parts(bits: Vec<u8>, num_hashes: u32, num_bits: u64) -> Self {
        Self {
            bits,
            num_hashes,
            num_bits,
        }
    }

    /// Insert a hash into the bloom filter.
    fn insert(&mut self, hash: &MerkleHash) {
        let (h1, h2) = Self::extract_hashes(hash);
        for i in 0..self.num_hashes {
            let bit_idx = h1.wrapping_add(u64::from(i).wrapping_mul(h2)) % self.num_bits;
            let byte_idx = (bit_idx / 8) as usize;
            let bit_offset = (bit_idx % 8) as u8;
            self.bits[byte_idx] |= 1 << bit_offset;
        }
    }

    /// Check if a hash might be in the filter. False positives possible;
    /// false negatives are not.
    fn maybe_contains(&self, hash: &MerkleHash) -> bool {
        if self.num_bits == 0 {
            return false;
        }
        let (h1, h2) = Self::extract_hashes(hash);
        for i in 0..self.num_hashes {
            let bit_idx = h1.wrapping_add(u64::from(i).wrapping_mul(h2)) % self.num_bits;
            let byte_idx = (bit_idx / 8) as usize;
            let bit_offset = (bit_idx % 8) as u8;
            if self.bits[byte_idx] & (1 << bit_offset) == 0 {
                return false;
            }
        }
        true
    }

    /// Extract two independent hash values from a `MerkleHash`.
    ///
    /// The hash is already 256 bits of blake3 output, so the four u64
    /// components are well-distributed. We use the first two as h1 and h2.
    fn extract_hashes(hash: &MerkleHash) -> (u64, u64) {
        // MerkleHash derefs to [u64; 4]. Use first two components.
        let h1 = hash[0];
        // Ensure h2 is odd so it's coprime with any power-of-two modulus,
        // giving better bit coverage.
        let h2 = hash[1] | 1;
        (h1, h2)
    }
}

impl ShardBloom {
    /// Create a new bloom from sets of file and chunk hashes.
    #[must_use]
    pub fn build(file_hashes: &[MerkleHash], chunk_hashes: &[MerkleHash]) -> Self {
        let mut file_bloom = BloomFilter::new(file_hashes.len());
        for h in file_hashes {
            file_bloom.insert(h);
        }

        let mut chunk_bloom = BloomFilter::new(chunk_hashes.len());
        for h in chunk_hashes {
            chunk_bloom.insert(h);
        }

        Self {
            file_bloom,
            chunk_bloom,
        }
    }

    /// Check if a file hash might be in this shard.
    #[must_use]
    pub fn maybe_contains_file(&self, hash: &MerkleHash) -> bool {
        self.file_bloom.maybe_contains(hash)
    }

    /// Check if a chunk hash might be in this shard.
    #[must_use]
    pub fn maybe_contains_chunk(&self, hash: &MerkleHash) -> bool {
        self.chunk_bloom.maybe_contains(hash)
    }

    /// Encode to bytes for appending to shard.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let file_data_len = self.file_bloom.bits.len();
        let chunk_data_len = self.chunk_bloom.bits.len();
        // Header: 4 (magic) + 1 (version) + 2 * (8 + 4 + 4) = 37 bytes fixed
        let total = 5 + 16 + file_data_len + 16 + chunk_data_len;
        let mut buf = Vec::with_capacity(total);

        // Header
        buf.extend_from_slice(BLOOM_MAGIC);
        buf.push(BLOOM_VERSION);

        // File bloom
        buf.extend_from_slice(&self.file_bloom.num_bits.to_le_bytes());
        buf.extend_from_slice(&self.file_bloom.num_hashes.to_le_bytes());
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bloom bit-vector length is bounded by element count × 10 bits, well under u32::MAX"
        )]
        let file_len_u32 = file_data_len as u32;
        buf.extend_from_slice(&file_len_u32.to_le_bytes());
        buf.extend_from_slice(&self.file_bloom.bits);

        // Chunk bloom
        buf.extend_from_slice(&self.chunk_bloom.num_bits.to_le_bytes());
        buf.extend_from_slice(&self.chunk_bloom.num_hashes.to_le_bytes());
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bloom bit-vector length is bounded by element count × 10 bits, well under u32::MAX"
        )]
        let chunk_len_u32 = chunk_data_len as u32;
        buf.extend_from_slice(&chunk_len_u32.to_le_bytes());
        buf.extend_from_slice(&self.chunk_bloom.bits);

        buf
    }

    /// Decode from bytes.
    ///
    /// # Errors
    /// Returns `XetError::CorruptObject` if the data is malformed.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let corrupt = |reason: &str| XetError::CorruptObject {
            path: "shard_bloom".to_owned(),
            reason: reason.to_owned(),
        };

        // Minimum: magic(4) + version(1) + 2 * (num_bits(8) + num_hashes(4) + data_len(4))
        if data.len() < 37 {
            return Err(corrupt("bloom data too short"));
        }

        if &data[..4] != BLOOM_MAGIC {
            return Err(corrupt("invalid bloom magic"));
        }
        if data[4] != BLOOM_VERSION {
            return Err(corrupt(&format!("unsupported bloom version {}", data[4])));
        }

        let mut pos = 5;

        let file_bloom = Self::decode_one_bloom(data, &mut pos)?;
        let chunk_bloom = Self::decode_one_bloom(data, &mut pos)?;

        Ok(Self {
            file_bloom,
            chunk_bloom,
        })
    }

    /// Decode a single `BloomFilter` from `data` starting at `pos`.
    fn decode_one_bloom(data: &[u8], pos: &mut usize) -> Result<BloomFilter> {
        let corrupt = |reason: &str| XetError::CorruptObject {
            path: "shard_bloom".to_owned(),
            reason: reason.to_owned(),
        };

        if *pos + 16 > data.len() {
            return Err(corrupt("bloom header truncated"));
        }

        let num_bits = u64::from_le_bytes(
            data[*pos..*pos + 8]
                .try_into()
                .map_err(|_| corrupt("bad num_bits"))?,
        );
        *pos += 8;

        let num_hashes = u32::from_le_bytes(
            data[*pos..*pos + 4]
                .try_into()
                .map_err(|_| corrupt("bad num_hashes"))?,
        );
        *pos += 4;

        let data_len = u32::from_le_bytes(
            data[*pos..*pos + 4]
                .try_into()
                .map_err(|_| corrupt("bad data_len"))?,
        ) as usize;
        *pos += 4;

        if *pos + data_len > data.len() {
            return Err(corrupt("bloom data truncated"));
        }

        let bits = data[*pos..*pos + data_len].to_vec();
        *pos += data_len;

        Ok(BloomFilter::from_parts(bits, num_hashes, num_bits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([
            seed,
            seed.wrapping_mul(31),
            seed.wrapping_mul(97),
            seed.wrapping_mul(127),
        ])
    }

    #[test]
    fn bloom_no_false_negatives() {
        let file_hashes: Vec<MerkleHash> = (1..=100).map(hash_from_seed).collect();
        let chunk_hashes: Vec<MerkleHash> = (200..=500).map(hash_from_seed).collect();

        let bloom = ShardBloom::build(&file_hashes, &chunk_hashes);

        for h in &file_hashes {
            assert!(bloom.maybe_contains_file(h), "false negative on file hash");
        }
        for h in &chunk_hashes {
            assert!(
                bloom.maybe_contains_chunk(h),
                "false negative on chunk hash"
            );
        }
    }

    #[test]
    fn bloom_encode_decode_round_trip() {
        let file_hashes: Vec<MerkleHash> = (1..=50).map(hash_from_seed).collect();
        let chunk_hashes: Vec<MerkleHash> = (100..=200).map(hash_from_seed).collect();

        let bloom = ShardBloom::build(&file_hashes, &chunk_hashes);
        let encoded = bloom.encode();
        let decoded = ShardBloom::decode(&encoded).expect("decode should succeed");

        // Verify no false negatives after round-trip.
        for h in &file_hashes {
            assert!(decoded.maybe_contains_file(h));
        }
        for h in &chunk_hashes {
            assert!(decoded.maybe_contains_chunk(h));
        }
    }

    #[test]
    fn bloom_empty_inputs() {
        let bloom = ShardBloom::build(&[], &[]);
        let encoded = bloom.encode();
        let decoded = ShardBloom::decode(&encoded).expect("decode should succeed");

        // Non-existent hash should (almost certainly) return false on empty bloom.
        let absent = hash_from_seed(999);
        assert!(!decoded.maybe_contains_file(&absent));
        assert!(!decoded.maybe_contains_chunk(&absent));
    }

    #[test]
    fn bloom_decode_rejects_bad_magic() {
        let mut data = ShardBloom::build(&[], &[]).encode();
        data[0] = b'X';
        assert!(ShardBloom::decode(&data).is_err());
    }

    #[test]
    fn bloom_decode_rejects_truncated_data() {
        assert!(ShardBloom::decode(&[]).is_err());
        assert!(ShardBloom::decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn bloom_file_and_chunk_are_independent() {
        let file_hashes: Vec<MerkleHash> = (1..=10).map(hash_from_seed).collect();
        let chunk_hashes: Vec<MerkleHash> = (1000..=1010).map(hash_from_seed).collect();

        let bloom = ShardBloom::build(&file_hashes, &chunk_hashes);

        // File hashes should not necessarily appear in chunk bloom and vice versa.
        // We can't guarantee false negatives on the *other* bloom, but we can
        // verify the inserted set is correct.
        for h in &file_hashes {
            assert!(bloom.maybe_contains_file(h));
        }
        for h in &chunk_hashes {
            assert!(bloom.maybe_contains_chunk(h));
        }
    }
}
