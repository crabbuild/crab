use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::StreamExt as _;
use s3s::dto::StreamingBlob;
use tokio::io::{AsyncWriteExt as _, BufWriter};

pub(crate) const MAX_PUT_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub(crate) const MAX_MULTIPART_OBJECT_BYTES: u64 = 50_000_000_000_000;
pub(crate) const MAX_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub(crate) const INLINE_GIT_BLOB_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("request body exceeds the operation limit")]
    TooLarge,
    #[error("request body length does not match Content-Length")]
    Incomplete,
    #[error("request body stream failed")]
    Body(#[source] s3s::StdError),
    #[error("content spool I/O failed")]
    Io(#[from] std::io::Error),
}

pub(crate) struct Digests {
    pub(crate) md5: [u8; 16],
    pub(crate) sha1: [u8; 20],
    pub(crate) sha256: [u8; 32],
    pub(crate) crc32: u32,
    pub(crate) crc32c: u32,
    pub(crate) crc64nvme: u64,
    pub(crate) blake3: [u8; 32],
}

pub(crate) struct Spool {
    _directory: tempfile::TempDir,
    path: PathBuf,
    pub(crate) size: u64,
    pub(crate) digests: Digests,
}

impl Spool {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) async fn bytes(&self) -> Result<Bytes, Error> {
        Ok(Bytes::from(tokio::fs::read(&self.path).await?))
    }
}

pub(crate) struct SpoolWriter {
    directory: tempfile::TempDir,
    path: PathBuf,
    file: BufWriter<tokio::fs::File>,
    size: u64,
    md5: md5::Md5,
    sha1: sha1::Sha1,
    sha256: sha2::Sha256,
    crc32: crc_fast::Digest,
    crc32c: crc_fast::Digest,
    crc64nvme: crc_fast::Digest,
    blake3: blake3::Hasher,
}

impl SpoolWriter {
    pub(crate) async fn new() -> Result<Self, Error> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("content");
        let file = BufWriter::new(tokio::fs::File::create(&path).await?);
        Ok(Self {
            directory,
            path,
            file,
            size: 0,
            md5: md5::Md5::default(),
            sha1: sha1::Sha1::default(),
            sha256: sha2::Sha256::default(),
            crc32: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32IsoHdlc),
            crc32c: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32Iscsi),
            crc64nvme: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc64Nvme),
            blake3: blake3::Hasher::new(),
        })
    }

    pub(crate) async fn write(&mut self, bytes: &[u8], max_bytes: u64) -> Result<(), Error> {
        let chunk_size = u64::try_from(bytes.len()).map_err(|_| Error::TooLarge)?;
        self.size = self.size.checked_add(chunk_size).ok_or(Error::TooLarge)?;
        if self.size > max_bytes {
            return Err(Error::TooLarge);
        }
        self.file.write_all(bytes).await?;
        md5::Digest::update(&mut self.md5, bytes);
        sha1::Digest::update(&mut self.sha1, bytes);
        sha2::Digest::update(&mut self.sha256, bytes);
        self.crc32.update(bytes);
        self.crc32c.update(bytes);
        self.crc64nvme.update(bytes);
        self.blake3.update(bytes);
        Ok(())
    }

    pub(crate) async fn finish(mut self) -> Result<Spool, Error> {
        self.file.flush().await?;
        drop(self.file);
        Ok(Spool {
            _directory: self.directory,
            path: self.path,
            size: self.size,
            digests: Digests {
                md5: md5::Digest::finalize(self.md5).into(),
                sha1: sha1::Digest::finalize(self.sha1).into(),
                sha256: sha2::Digest::finalize(self.sha256).into(),
                crc32: u32::try_from(self.crc32.finalize()).map_err(|_| Error::TooLarge)?,
                crc32c: u32::try_from(self.crc32c.finalize()).map_err(|_| Error::TooLarge)?,
                crc64nvme: self.crc64nvme.finalize(),
                blake3: *self.blake3.finalize().as_bytes(),
            },
        })
    }
}

pub(crate) async fn spool_body(
    body: Option<StreamingBlob>,
    declared: Option<i64>,
    max_bytes: u64,
) -> Result<Spool, Error> {
    let declared = declared
        .map(|length| u64::try_from(length).map_err(|_| Error::Incomplete))
        .transpose()?;
    if declared.is_some_and(|length| length > max_bytes) {
        return Err(Error::TooLarge);
    }
    let mut writer = SpoolWriter::new().await?;
    if let Some(mut body) = body {
        while let Some(chunk) = body.next().await {
            writer
                .write(&chunk.map_err(Error::Body)?, max_bytes)
                .await?;
        }
    }
    if declared.is_some_and(|length| length != writer.size) {
        return Err(Error::Incomplete);
    }
    writer.finish().await
}

pub(crate) fn md5_hex(digest: &[u8; 16]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_limits_match_s3_general_purpose_buckets() {
        assert_eq!(
            (
                MAX_PUT_OBJECT_BYTES,
                MAX_MULTIPART_PART_BYTES,
                MAX_MULTIPART_OBJECT_BYTES,
            ),
            (
                5 * 1024 * 1024 * 1024,
                5 * 1024 * 1024 * 1024,
                50_000_000_000_000,
            )
        );
    }

    #[tokio::test]
    async fn spool_writer_hashes_chunks_and_enforces_limit() {
        use sha1::Digest as _;

        let mut writer = SpoolWriter::new().await.unwrap();
        writer.write(b"streamed ", 16).await.unwrap();
        writer.write(b"content", 16).await.unwrap();
        let spool = writer.finish().await.unwrap();
        let expected_md5: [u8; 16] = <md5::Md5 as md5::Digest>::digest(b"streamed content").into();
        let expected_sha1: [u8; 20] = sha1::Sha1::digest(b"streamed content").into();
        let expected_sha256: [u8; 32] = sha2::Sha256::digest(b"streamed content").into();
        assert_eq!(
            (
                spool.size,
                spool.digests.md5,
                spool.digests.sha1,
                spool.digests.sha256,
            ),
            (16, expected_md5, expected_sha1, expected_sha256)
        );
    }

    #[tokio::test]
    async fn spool_writer_rejects_content_over_the_operation_limit() {
        let mut writer = SpoolWriter::new().await.unwrap();
        assert!(matches!(
            writer.write(b"too large", 8).await,
            Err(Error::TooLarge)
        ));
    }
}
