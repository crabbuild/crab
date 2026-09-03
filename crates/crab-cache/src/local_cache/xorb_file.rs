use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::private_fs::{check_cancelled, run_blocking};
use crab_xet::xorb::builder::FOOTER_SIZE;
use crab_xet::xorb::parser::{
    verify_compressed_chunk, xorb_chunks_from_metadata, xorb_metadata_region,
    xorb_payload_digest_from_footer,
};

struct Metadata {
    chunks: Vec<ChunkMeta>,
    hash: MerkleHash,
    payload_digest: [u8; 32],
}

pub(super) async fn read_xorb_file_metadata(
    file: tokio::fs::File,
    path: &Path,
    file_len: u64,
) -> Result<(Vec<ChunkMeta>, MerkleHash)> {
    let mut file = file.into_std().await;
    let path = path.to_owned();
    run_blocking(&CancellationToken::new(), move |cancel| {
        let parsed = metadata(&mut file, &path, file_len, cancel)?;
        Ok((parsed.chunks, parsed.hash))
    })
    .await
}

pub(super) async fn verify_xorb_file_payload(
    file: tokio::fs::File,
    path: &Path,
    file_len: u64,
    expected: &MerkleHash,
) -> Result<()> {
    // Transfer the descriptor rather than cloning its shared seek cursor.
    // A cancelled async caller cannot reuse a file still owned by this worker.
    let mut file = file.into_std().await;
    let path = path.to_owned();
    let expected = *expected;
    run_blocking(&CancellationToken::new(), move |cancel| {
        verify(&mut file, &path, file_len, &expected, cancel)
    })
    .await
}

pub(super) async fn verify_xorb_file_identity(
    file: tokio::fs::File,
    path: &Path,
    file_len: u64,
    expected: &MerkleHash,
) -> Result<tokio::fs::File> {
    let mut file = file.into_std().await;
    let path = path.to_owned();
    let expected = *expected;
    let file = run_blocking(&CancellationToken::new(), move |cancel| {
        let parsed = metadata(&mut file, &path, file_len, cancel)?;
        if parsed.hash != expected {
            return Err(CacheError::HashMismatch {
                requested: expected.hex(),
                actual: parsed.hash.hex(),
            });
        }
        Ok(file)
    })
    .await?;
    Ok(tokio::fs::File::from_std(file))
}

pub(super) fn verify(
    file: &mut File,
    path: &Path,
    file_len: u64,
    expected: &MerkleHash,
    cancel: &CancellationToken,
) -> Result<()> {
    let parsed = metadata(file, path, file_len, cancel)?;
    if parsed.hash != *expected {
        return Err(CacheError::HashMismatch {
            requested: expected.hex(),
            actual: parsed.hash.hex(),
        });
    }
    file.seek(SeekFrom::Start(0))?;
    let mut digest = blake3::Hasher::new();
    for chunk in &parsed.chunks {
        let compressed = read_bytes(file, chunk.compressed_len as usize, cancel)?;
        digest.update(&compressed);
        verify_compressed_chunk(chunk, &compressed)?;
    }
    check_cancelled(cancel)?;
    if digest.finalize().as_bytes() != &parsed.payload_digest {
        return Err(corrupt(path, "serialized payload digest mismatch"));
    }
    if file.metadata()?.len() != file_len {
        return Err(corrupt(path, "xorb changed during verification"));
    }
    Ok(())
}

fn metadata(
    file: &mut File,
    path: &Path,
    file_len: u64,
    cancel: &CancellationToken,
) -> Result<Metadata> {
    check_cancelled(cancel)?;
    if file_len > MAX_XORB_SIZE as u64 || file_len < FOOTER_SIZE as u64 {
        return Err(corrupt(path, "xorb length is outside format limits"));
    }
    if file.metadata()?.len() != file_len {
        return Err(corrupt(path, "xorb changed during verification"));
    }
    let file_len =
        usize::try_from(file_len).map_err(|_| corrupt(path, "xorb length does not fit usize"))?;
    file.seek(SeekFrom::Start((file_len - FOOTER_SIZE) as u64))?;
    let footer = read_bytes(file, FOOTER_SIZE, cancel)?;
    let region = xorb_metadata_region(file_len, &footer)?;
    let payload_digest = xorb_payload_digest_from_footer(file_len, &footer)?;
    file.seek(SeekFrom::Start(region.offset as u64))?;
    let bytes = read_bytes(file, region.len, cancel)?;
    let (chunks, hash) = xorb_chunks_from_metadata(file_len, &footer, &bytes)?;
    Ok(Metadata {
        chunks,
        hash,
        payload_digest,
    })
}

fn read_bytes(file: &mut File, size: usize, cancel: &CancellationToken) -> Result<Vec<u8>> {
    check_cancelled(cancel)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::OutOfMemory, error))?;
    bytes.resize(size, 0);
    for block in bytes.chunks_mut(64 * 1024) {
        check_cancelled(cancel)?;
        file.read_exact(block)?;
    }
    Ok(bytes)
}

fn corrupt(path: &Path, reason: &str) -> CacheError {
    CacheError::CorruptObject {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}
