//! Per-file async chunker driver.
//!
//! Reads from an [`AsyncRead`] source in 128 KiB buffers, then offloads
//! the CPU-bound gearhash CDC work to [`tokio::task::spawn_blocking`].
//! A running BLAKE3 file hash is computed over the raw bytes so callers
//! get both the chunk list and the content-addressed file identity in a
//! single pass.

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::core::error::Result;
use crab_xet::chunker::{Chunk, GearChunker};
use crab_xet::hash::MerkleHash;

/// Read buffer size: 128 KiB.
const READ_BUF_SIZE: usize = 128 * 1024;

/// Result of chunking an entire file stream.
pub struct ChunkResult {
    /// Content-defined chunks produced by gearhash CDC.
    pub chunks: Vec<Chunk>,
    /// Raw BLAKE3 hash over the complete file contents.
    pub file_hash: MerkleHash,
    /// Total bytes read from the source.
    pub total_bytes: u64,
}

/// Chunks an entire async byte stream, returning chunks and a file hash.
///
/// Reads from `reader` in 128 KiB buffers, accumulates the raw bytes,
/// then offloads gearhash CDC and blake3 hashing to
/// [`tokio::task::spawn_blocking`] so the tokio runtime stays
/// responsive.
///
/// # Errors
///
/// Returns [`CrabError::Io`] on read failure and
/// [`CrabError::Internal`] if the blocking task panics.
pub async fn chunk_file<R: AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
) -> Result<ChunkResult> {
    // Accumulate all bytes on the async side — IO-bound work stays on
    // the runtime.  A typical large file is tens of MiB; holding it in
    // memory is fine and avoids the complexity of shuttling GearChunker
    // state in and out of spawn_blocking.
    let mut data = Vec::new();
    let mut buf = vec![0u8; READ_BUF_SIZE];

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }

    let total_bytes = data.len() as u64;

    // CPU-bound: gearhash chunking + BLAKE3 file hash.
    let result = tokio::task::spawn_blocking(move || {
        let file_hash = MerkleHash::from(*blake3::hash(&data).as_bytes());

        let mut chunker = GearChunker::new();
        let mut chunks = Vec::new();

        for block in data.chunks(READ_BUF_SIZE) {
            chunks.extend(chunker.feed(block));
        }
        if let Some(last) = chunker.finalize() {
            chunks.push(last);
        }

        ChunkResult {
            chunks,
            file_hash,
            total_bytes,
        }
    })
    .await
    .map_err(|e| crate::core::error::CrabError::Internal(e.to_string()))?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[tokio::test]
    async fn empty_reader_produces_no_chunks() {
        let result = chunk_file(Cursor::new(Vec::new()))
            .await
            .expect("should succeed");
        assert!(result.chunks.is_empty());
        assert_eq!(result.total_bytes, 0);
    }

    #[tokio::test]
    async fn small_input_produces_single_chunk() {
        let data = vec![42u8; 1024];
        let expected_hash = MerkleHash::from(*blake3::hash(&data).as_bytes());
        let result = chunk_file(Cursor::new(data)).await.expect("should succeed");
        assert_eq!(result.total_bytes, 1024);
        assert_eq!(result.chunks.len(), 1);
        assert_eq!(result.chunks[0].data.len(), 1024);
        assert_eq!(result.file_hash, expected_hash);
    }

    #[tokio::test]
    async fn large_input_produces_multiple_chunks() {
        let data: Vec<u8> = (0..1_048_576u32)
            .map(|i| (i.wrapping_mul(2_654_435_761)) as u8)
            .collect();
        let expected_hash = MerkleHash::from(*blake3::hash(&data).as_bytes());
        let result = chunk_file(Cursor::new(data.clone()))
            .await
            .expect("should succeed");
        assert_eq!(result.total_bytes, data.len() as u64);
        assert!(
            result.chunks.len() > 1,
            "expected multiple chunks from 1 MiB"
        );

        let reassembled: Vec<u8> = result
            .chunks
            .iter()
            .flat_map(|c| c.data.iter().copied())
            .collect();
        assert_eq!(reassembled, data);
        assert_eq!(result.file_hash, expected_hash);
    }

    #[tokio::test]
    async fn file_hash_is_deterministic() {
        let data = vec![7u8; 300_000];
        let expected_hash = MerkleHash::from(*blake3::hash(&data).as_bytes());
        let r1 = chunk_file(Cursor::new(data.clone())).await.expect("r1");
        let r2 = chunk_file(Cursor::new(data)).await.expect("r2");
        assert_eq!(r1.file_hash, expected_hash);
        assert_eq!(r1.file_hash, r2.file_hash);
        assert_eq!(r1.chunks.len(), r2.chunks.len());
    }

    #[tokio::test]
    async fn file_hash_bytes_match_clean_pointer_contract() {
        let data = b"import and clean must agree on pointer hash bytes".to_vec();
        let expected_bytes: [u8; 32] = *blake3::hash(&data).as_bytes();
        let result = chunk_file(Cursor::new(data)).await.expect("should succeed");
        let actual_bytes: [u8; 32] = result.file_hash.into();

        assert_eq!(actual_bytes, expected_bytes);
    }

    #[tokio::test]
    async fn chunk_boundaries_match_direct_chunker() {
        let data: Vec<u8> = (0..512_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761)) as u8)
            .collect();

        // Direct chunker (single feed).
        let mut direct = GearChunker::new();
        let mut direct_chunks = direct.feed(&data);
        if let Some(last) = direct.finalize() {
            direct_chunks.push(last);
        }

        let result = chunk_file(Cursor::new(data)).await.expect("should succeed");

        assert_eq!(result.chunks.len(), direct_chunks.len());
        for (a, b) in result.chunks.iter().zip(direct_chunks.iter()) {
            assert_eq!(a.data, b.data);
            assert_eq!(a.hash, b.hash);
        }
    }
}
