use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use bytes::Bytes;
use futures_util::StreamExt as _;
use s3s::dto::StreamingBlob;
use tokio::io::{AsyncWriteExt as _, BufWriter};

use crate::metrics::{
    Metrics, ScratchCapacityError, ScratchFailure, ScratchPurpose, ScratchReservation, ScratchUsage,
};

pub(crate) const MAX_PUT_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub(crate) const MAX_MULTIPART_OBJECT_BYTES: u64 = 50_000_000_000_000;
pub(crate) const MAX_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub(crate) const INLINE_GIT_BLOB_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct ResponseIdleTimeout;

impl std::fmt::Display for ResponseIdleTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("response body was idle for too long")
    }
}

impl std::error::Error for ResponseIdleTimeout {}

pub(crate) fn max_multipart_object_bytes(provider: crab_storage::StorageProviderKind) -> u64 {
    MAX_MULTIPART_OBJECT_BYTES.min(crab_storage::multipart::upload_limits(provider).max_object_size)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("request body exceeds the operation limit")]
    TooLarge,
    #[error("request body length does not match Content-Length")]
    Incomplete,
    #[error("request body stream failed")]
    Body(#[source] s3s::StdError),
    #[error("request body was idle for too long")]
    BodyTimeout,
    #[error("content spool I/O failed")]
    Io(#[from] std::io::Error),
    #[error("content spool capacity is unavailable")]
    Capacity(#[from] ScratchCapacityError),
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

pub(crate) struct Digester {
    size: u64,
    md5: md5::Md5,
    sha1: sha1::Sha1,
    sha256: sha2::Sha256,
    crc32: crc_fast::Digest,
    crc32c: crc_fast::Digest,
    crc64nvme: crc_fast::Digest,
    blake3: blake3::Hasher,
}

impl Digester {
    pub(crate) fn new() -> Self {
        Self {
            size: 0,
            md5: md5::Md5::default(),
            sha1: sha1::Sha1::default(),
            sha256: sha2::Sha256::default(),
            crc32: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32IsoHdlc),
            crc32c: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32Iscsi),
            crc64nvme: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc64Nvme),
            blake3: blake3::Hasher::new(),
        }
    }

    pub(crate) fn write(&mut self, bytes: &[u8], max_bytes: u64) -> Result<(), Error> {
        let chunk_size = u64::try_from(bytes.len()).map_err(|_| Error::TooLarge)?;
        self.size = self.size.checked_add(chunk_size).ok_or(Error::TooLarge)?;
        if self.size > max_bytes {
            return Err(Error::TooLarge);
        }
        md5::Digest::update(&mut self.md5, bytes);
        sha1::Digest::update(&mut self.sha1, bytes);
        sha2::Digest::update(&mut self.sha256, bytes);
        self.crc32.update(bytes);
        self.crc32c.update(bytes);
        self.crc64nvme.update(bytes);
        self.blake3.update(bytes);
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<(u64, Digests), Error> {
        Ok((
            self.size,
            Digests {
                md5: md5::Digest::finalize(self.md5).into(),
                sha1: sha1::Digest::finalize(self.sha1).into(),
                sha256: sha2::Digest::finalize(self.sha256).into(),
                crc32: u32::try_from(self.crc32.finalize()).map_err(|_| Error::TooLarge)?,
                crc32c: u32::try_from(self.crc32c.finalize()).map_err(|_| Error::TooLarge)?,
                crc64nvme: self.crc64nvme.finalize(),
                blake3: *self.blake3.finalize().as_bytes(),
            },
        ))
    }
}

pub(crate) struct Spool {
    _directory: tempfile::TempDir,
    path: PathBuf,
    // Accounting follows the temporary directory across writer-to-spool ownership.
    // Dropping either side releases both the file and its metric charge.
    scratch: ScratchUsage,
    pub(crate) size: u64,
    pub(crate) digests: Digests,
}

impl Spool {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) async fn bytes(&self) -> Result<Bytes, Error> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => Ok(Bytes::from(bytes)),
            Err(error) => {
                self.scratch.record_failure(ScratchFailure::Read);
                Err(error.into())
            }
        }
    }
}

pub(crate) struct SpoolWriter {
    directory: tempfile::TempDir,
    path: PathBuf,
    file: BufWriter<tokio::fs::File>,
    digester: Digester,
    reservation: ScratchReservation,
    scratch: ScratchUsage,
}

impl SpoolWriter {
    pub(crate) async fn new(metrics: &Metrics, expected_bytes: Option<u64>) -> Result<Self, Error> {
        let reservation = metrics.reserve_scratch(expected_bytes.unwrap_or(0))?;
        let scratch = metrics.start_scratch(ScratchPurpose::ContentSpool);
        let directory = tempfile::tempdir().inspect_err(|_| {
            scratch.record_failure(ScratchFailure::Create);
        })?;
        let path = directory.path().join("content");
        let file = tokio::fs::File::create(&path).await.inspect_err(|_| {
            scratch.record_failure(ScratchFailure::Create);
        })?;
        Ok(Self {
            directory,
            path,
            file: BufWriter::new(file),
            digester: Digester::new(),
            reservation,
            scratch,
        })
    }

    pub(crate) async fn write(&mut self, bytes: &[u8], max_bytes: u64) -> Result<(), Error> {
        self.digester.write(bytes, max_bytes)?;
        let size = u64::try_from(bytes.len()).map_err(|_| Error::TooLarge)?;
        let capacity = self.reservation.reserve_write(size)?;
        self.scratch.reserve(size);
        if let Err(error) = self.file.write_all(bytes).await {
            self.scratch.record_failure(ScratchFailure::Write);
            return Err(error.into());
        }
        self.scratch.record_written(size);
        drop(capacity);
        Ok(())
    }

    pub(crate) fn size(&self) -> u64 {
        self.digester.size
    }

    pub(crate) async fn finish(mut self) -> Result<Spool, Error> {
        if let Err(error) = self.file.flush().await {
            self.scratch.record_failure(ScratchFailure::Flush);
            return Err(error.into());
        }
        drop(self.file);
        let (size, digests) = self.digester.finish()?;
        Ok(Spool {
            _directory: self.directory,
            path: self.path,
            scratch: self.scratch,
            size,
            digests,
        })
    }
}

pub(crate) async fn spool_body(
    body: Option<StreamingBlob>,
    declared: Option<i64>,
    max_bytes: u64,
    metrics: &Metrics,
) -> Result<Spool, Error> {
    spool_body_with_timeout(body, declared, max_bytes, metrics, BODY_IDLE_TIMEOUT).await
}

async fn spool_body_with_timeout(
    body: Option<StreamingBlob>,
    declared: Option<i64>,
    max_bytes: u64,
    metrics: &Metrics,
    idle_timeout: Duration,
) -> Result<Spool, Error> {
    let declared = declared
        .map(|length| u64::try_from(length).map_err(|_| Error::Incomplete))
        .transpose()?;
    if declared.is_some_and(|length| length > max_bytes) {
        return Err(Error::TooLarge);
    }
    let mut writer = SpoolWriter::new(metrics, declared).await?;
    if let Some(mut body) = body {
        while let Some(chunk) = tokio::time::timeout(idle_timeout, body.next())
            .await
            .map_err(|_| Error::BodyTimeout)?
        {
            writer
                .write(&chunk.map_err(Error::Body)?, max_bytes)
                .await?;
        }
    }
    if declared.is_some_and(|length| length != writer.size()) {
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

    #[test]
    fn completed_object_limit_respects_the_physical_backend() {
        assert_eq!(
            max_multipart_object_bytes(crab_storage::StorageProviderKind::S3),
            MAX_MULTIPART_OBJECT_BYTES
        );
        assert_eq!(
            max_multipart_object_bytes(crab_storage::StorageProviderKind::Azure),
            MAX_MULTIPART_OBJECT_BYTES
        );
        assert_eq!(
            max_multipart_object_bytes(crab_storage::StorageProviderKind::Gcs),
            5 * 1024_u64.pow(4)
        );
    }

    #[tokio::test]
    async fn spool_writer_hashes_chunks_and_enforces_limit() {
        use sha1::Digest as _;

        let metrics = Metrics::new().unwrap();
        let mut writer = SpoolWriter::new(&metrics, None).await.unwrap();
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

    #[test]
    fn digester_hashes_without_creating_a_spool() {
        let mut digester = Digester::new();
        digester.write(b"streamed ", 16).unwrap();
        digester.write(b"content", 16).unwrap();
        let (size, digests) = digester.finish().unwrap();
        let expected_sha256: [u8; 32] =
            <sha2::Sha256 as sha2::Digest>::digest(b"streamed content").into();

        assert_eq!(size, 16);
        assert_eq!(digests.sha256, expected_sha256);
    }

    #[tokio::test]
    async fn spool_writer_rejects_content_over_the_operation_limit() {
        let metrics = Metrics::new().unwrap();
        let mut writer = SpoolWriter::new(&metrics, None).await.unwrap();
        assert!(matches!(
            writer.write(b"too large", 8).await,
            Err(Error::TooLarge)
        ));
    }

    #[tokio::test]
    async fn declared_body_reserves_capacity_before_polling_the_stream() {
        let metrics = Metrics::new().unwrap();
        let body = StreamingBlob::wrap(futures_util::stream::poll_fn(
            |_| -> std::task::Poll<Option<std::result::Result<Bytes, std::io::Error>>> {
                panic!("body must not be polled when its capacity reservation is rejected")
            },
        ));

        let result = spool_body(Some(body), Some(i64::MAX), u64::MAX, &metrics).await;

        assert!(matches!(
            result,
            Err(Error::Capacity(ScratchCapacityError::Exhausted))
        ));
    }

    #[tokio::test]
    async fn idle_body_timeout_releases_the_spool() {
        let metrics = Metrics::new().unwrap();
        let body = StreamingBlob::wrap(futures_util::stream::pending::<
            std::result::Result<Bytes, std::io::Error>,
        >());

        let result = spool_body_with_timeout(
            Some(body),
            None,
            u64::MAX,
            &metrics,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert!(matches!(result, Err(Error::BodyTimeout)));
        assert!(
            metrics
                .render(&crate::admission::Admission::new(
                    8,
                    tokio_util::sync::CancellationToken::new(),
                    metrics.clone(),
                ))
                .contains("crab_s3_gateway_scratch_files{purpose=\"content_spool\"} 0")
        );
    }

    #[tokio::test]
    async fn spool_metrics_follow_the_temporary_file_lifetime() {
        let metrics = Metrics::new().unwrap();
        let admission = crate::admission::Admission::new(
            8,
            tokio_util::sync::CancellationToken::new(),
            metrics.clone(),
        );
        let mut writer = SpoolWriter::new(&metrics, None).await.unwrap();
        writer.write(b"scratch", 16).await.unwrap();
        let spool = writer.finish().await.unwrap();

        let active = metrics.render(&admission);
        assert!(active.contains("crab_s3_gateway_scratch_files{purpose=\"content_spool\"} 1"));
        assert!(active.contains("crab_s3_gateway_scratch_bytes{purpose=\"content_spool\"} 7"));
        drop(spool);
        let released = metrics.render(&admission);
        assert!(released.contains("crab_s3_gateway_scratch_files{purpose=\"content_spool\"} 0"));
        assert!(released.contains("crab_s3_gateway_scratch_bytes{purpose=\"content_spool\"} 0"));
    }
}
