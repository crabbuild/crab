pub use xet_core_structures::merklehash::{HashedWrite, MerkleHash, compute_data_hash, xorb_hash};

/// Convert raw Blake3 bytes to the Xet protocol's `MerkleHash::hex()` format.
///
/// `MerkleHash::hex()` reinterprets the 32 bytes as `[u64; 4]` and formats
/// each word with little-endian byte order, producing a different hex string
/// than raw byte-by-byte encoding.
#[must_use]
pub fn merkle_hex_from_bytes(bytes: &[u8; 32]) -> String {
    MerkleHash::from(*bytes).hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hash() -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = i as u8;
        }
        h
    }

    fn raw_hex(bytes: &[u8; 32]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    #[test]
    fn merkle_hex_uses_xet_word_order() {
        let bytes = sample_hash();
        let raw = raw_hex(&bytes);
        let merkle = merkle_hex_from_bytes(&bytes);

        assert_eq!(raw.len(), 64);
        assert_eq!(merkle.len(), 64);
        assert_ne!(raw, merkle);
    }
}
