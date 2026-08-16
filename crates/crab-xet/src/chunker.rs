//! Content-defined chunking via gearhash.

use bytes::Bytes;

pub use xet_data::deduplication::Chunk;
pub use xet_data::deduplication::constants::TARGET_CHUNK_SIZE;

use xet_data::deduplication::Chunker;

/// Content-defined chunker backed by gearhash rolling hash.
///
/// Wraps [`xet_data::deduplication::Chunker`] with convenience methods for
/// feeding data incrementally and finalizing the stream.
pub struct GearChunker {
    inner: Chunker,
}

impl Default for GearChunker {
    fn default() -> Self {
        Self::new()
    }
}

impl GearChunker {
    /// Creates a chunker with the default 64 KiB target chunk size.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Chunker::default(),
        }
    }

    /// Feeds a byte slice and returns any complete chunks produced.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Chunk> {
        self.inner.next_block(input, false)
    }

    /// Zero-copy variant of [`feed`](Self::feed) using [`Bytes`].
    pub fn feed_bytes(&mut self, input: &Bytes) -> Vec<Chunk> {
        self.inner.next_block_bytes(input, false)
    }

    /// Flushes any buffered data as a final chunk.
    #[must_use]
    pub fn finalize(mut self) -> Option<Chunk> {
        self.inner.finish()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn chunk_signature(data: &[u8], feed_sizes: &[usize]) -> Vec<(usize, String)> {
        let mut chunker = GearChunker::new();
        let mut chunks = Vec::new();
        let mut offset = 0usize;
        for size in feed_sizes {
            let end = offset.saturating_add(*size).min(data.len());
            chunks.extend(chunker.feed(&data[offset..end]));
            offset = end;
        }
        if offset < data.len() {
            chunks.extend(chunker.feed(&data[offset..]));
        }
        if let Some(last) = chunker.finalize() {
            chunks.push(last);
        }
        chunks
            .into_iter()
            .map(|chunk| (chunk.data.len(), chunk.hash.hex()))
            .collect()
    }

    fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn signature_summary(data: &[u8]) -> (usize, String) {
        let signature = chunk_signature(data, &[data.len()]);
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab gearhash-v1 conformance\0");
        for (size, hash) in &signature {
            hasher.update(&(*size as u64).to_le_bytes());
            hasher.update(hash.as_bytes());
        }
        (signature.len(), hasher.finalize().to_hex().to_string())
    }

    #[test]
    fn empty_input_produces_no_chunks() {
        let mut chunker = GearChunker::new();
        assert!(chunker.feed(&[]).is_empty());
        assert!(chunker.finalize().is_none());
    }

    #[test]
    fn small_input_buffered_until_finalize() {
        let mut chunker = GearChunker::new();
        let data = vec![42u8; 1024];
        let chunks = chunker.feed(&data);

        assert!(chunks.is_empty());
        let final_chunk = chunker.finalize();
        assert!(final_chunk.is_some());
        assert_eq!(final_chunk.unwrap().data.len(), 1024);
    }

    #[test]
    fn large_input_produces_multiple_chunks() {
        let mut chunker = GearChunker::new();
        let data: Vec<u8> = (0..1_048_576u32)
            .map(|i| (i.wrapping_mul(2_654_435_761)) as u8)
            .collect();
        let mut chunks = chunker.feed(&data);
        if let Some(last) = chunker.finalize() {
            chunks.push(last);
        }

        assert!(
            chunks.len() > 1,
            "expected multiple chunks from 1 MiB input"
        );
        let reassembled: Vec<u8> = chunks.iter().flat_map(|c| c.data.iter().copied()).collect();
        assert_eq!(reassembled, data);
    }

    #[test]
    fn feed_bytes_matches_feed() {
        let data: Vec<u8> = (0..512_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761)) as u8)
            .collect();
        let bytes_data = Bytes::from(data.clone());

        let mut chunker_slice = GearChunker::new();
        let mut chunks_slice = chunker_slice.feed(&data);
        if let Some(last) = chunker_slice.finalize() {
            chunks_slice.push(last);
        }

        let mut chunker_bytes = GearChunker::new();
        let mut chunks_bytes = chunker_bytes.feed_bytes(&bytes_data);
        if let Some(last) = chunker_bytes.finalize() {
            chunks_bytes.push(last);
        }

        assert_eq!(chunks_slice.len(), chunks_bytes.len());
        for (a, b) in chunks_slice.iter().zip(chunks_bytes.iter()) {
            assert_eq!(a.data, b.data);
            assert_eq!(a.hash, b.hash);
        }
    }

    #[test]
    fn feed_bytes_shares_complete_chunks_with_input_storage() {
        let data = Bytes::from(deterministic_bytes(4 * 1024 * 1024, 0x4352_4142_2d5a_4552));
        let input_start = data.as_ptr() as usize;
        let input_end = input_start + data.len();

        let mut chunker = GearChunker::new();
        let chunks = chunker.feed_bytes(&data);

        assert!(!chunks.is_empty());
        for chunk in chunks {
            let chunk_start = chunk.data.as_ptr() as usize;
            let chunk_end = chunk_start + chunk.data.len();
            assert!(chunk_start >= input_start);
            assert!(chunk_end <= input_end);
        }
    }

    #[test]
    fn incremental_feed_matches_single_feed() {
        let data: Vec<u8> = (0..256_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761)) as u8)
            .collect();

        let mut single = GearChunker::new();
        let mut single_chunks = single.feed(&data);
        if let Some(last) = single.finalize() {
            single_chunks.push(last);
        }

        let mut incremental = GearChunker::new();
        let mut inc_chunks = Vec::new();
        for block in data.chunks(4096) {
            inc_chunks.extend(incremental.feed(block));
        }
        if let Some(last) = incremental.finalize() {
            inc_chunks.push(last);
        }

        assert_eq!(single_chunks.len(), inc_chunks.len());
        for (a, b) in single_chunks.iter().zip(inc_chunks.iter()) {
            assert_eq!(a.data, b.data);
        }
    }

    #[test]
    fn gearhash_v1_conformance_fixtures() {
        let boundary = deterministic_bytes(1024 * 1024 + 17, 0x4352_4142_2d56_3101);
        let repeated_block = deterministic_bytes(256 * 1024, 0x4352_4142_2d56_3102);
        let repeated = repeated_block.repeat(8);
        let large = deterministic_bytes(8 * 1024 * 1024 + 31, 0x4352_4142_2d56_3103);

        let fixtures = [
            (
                "boundary",
                boundary.as_slice(),
                (
                    14,
                    "558f0f0c5c8100a9bf01e2a799b97cf1a5b9bb6c0b4feb4e165124f1026af7e5",
                ),
            ),
            (
                "repeated",
                repeated.as_slice(),
                (
                    41,
                    "581ed6183a37967a530db29ce9a9126cceaf0760aa154216647bceff869e1c41",
                ),
            ),
            (
                "large",
                large.as_slice(),
                (
                    133,
                    "00170704d321b616e916bd449f876ed4dd3f06c0594c70d8e5f56da1b1fe5806",
                ),
            ),
        ];

        for (name, data, expected) in fixtures {
            assert_eq!(
                signature_summary(data),
                (expected.0, expected.1.to_owned()),
                "{name}"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn feed_partition_does_not_change_gearhash_v1_recipe(
            data in proptest::collection::vec(any::<u8>(), 0..524_288),
            requested_parts in proptest::collection::vec(0usize..131_072, 0..128),
        ) {
            let expected = chunk_signature(&data, &[data.len()]);
            let actual = chunk_signature(&data, &requested_parts);
            prop_assert_eq!(actual, expected);
        }
    }
}
