//! Self-contained Git pack production from verified remote objects.

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};

use flate2::{Compression, write::ZlibEncoder};
use gix_hash::ObjectId;
use sha1::{Digest, Sha1};
use tempfile::NamedTempFile;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::{BudgetDimension, Error, OperationKind, RemoteGitObject, RemoteGitRepository, Result};

// Large locator batches let the reader coalesce adjacent pack ranges across the
// whole batch. The operation's object and byte budgets still bound residency.
const OBJECT_BATCH_SIZE: usize = 10_000;
const SIDEBAND_PAYLOAD: usize = 65_515;

/// A temporary, checksummed Git pack generated from one pinned snapshot.
pub struct GeneratedPack {
    file: NamedTempFile,
    size: u64,
    checksum: [u8; 20],
    object_count: u32,
}

impl std::fmt::Debug for GeneratedPack {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GeneratedPack")
            .field("size", &self.size)
            .field("object_count", &self.object_count)
            .finish()
    }
}

impl GeneratedPack {
    /// Return the temporary pack path while this generated pack is alive.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        self.file.path()
    }

    /// Return the complete pack size, including its trailing checksum.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Return the number of objects in the pack.
    #[must_use]
    pub const fn object_count(&self) -> u32 {
        self.object_count
    }

    /// Return the pack checksum.
    #[must_use]
    pub const fn checksum(&self) -> [u8; 20] {
        self.checksum
    }

    /// Return the pack checksum as lowercase hexadecimal text.
    #[must_use]
    pub fn checksum_hex(&self) -> String {
        self.checksum
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn verify_checksum(&self) -> Result<()> {
        let mut file = std::fs::File::open(self.file.path()).map_err(io_error)?;
        if self.size < 20 {
            return Err(Error::Metadata(
                crab_metadata::error::MetadataError::CorruptObject {
                    path: self.file.path().display().to_string(),
                    reason: "generated pack is shorter than its checksum".to_owned(),
                },
            ));
        }
        let mut body = Read::by_ref(&mut file).take(self.size - 20);
        let mut hash = Sha1::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let read = body.read(&mut chunk).map_err(io_error)?;
            if read == 0 {
                break;
            }
            hash.update(&chunk[..read]);
        }
        let mut trailer = [0u8; 20];
        file.read_exact(&mut trailer).map_err(io_error)?;
        let actual: [u8; 20] = hash.finalize().into();
        if actual != trailer || actual != self.checksum {
            return Err(Error::Metadata(
                crab_metadata::error::MetadataError::CorruptObject {
                    path: self.file.path().display().to_string(),
                    reason: "generated pack checksum verification failed".to_owned(),
                },
            ));
        }
        Ok(())
    }

    /// Stream the pack through protocol-v2 sideband channel 1.
    pub async fn write_sideband<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut file = tokio::fs::File::open(self.file.path())
            .await
            .map_err(io_error)?;
        let mut chunk = vec![0u8; SIDEBAND_PAYLOAD];
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let read = tokio::io::AsyncReadExt::read(&mut file, &mut chunk)
                .await
                .map_err(io_error)?;
            if read == 0 {
                break;
            }
            write_packet(writer, &chunk[..read], Some(1), cancellation).await?;
        }
        Ok(())
    }
}

impl RemoteGitRepository {
    /// Generate a self-contained pack from verified object IDs in this pinned
    /// repository generation. Objects are read and written in bounded batches.
    pub async fn generate_pack(
        &self,
        object_ids: &[ObjectId],
        cancellation: &CancellationToken,
    ) -> Result<GeneratedPack> {
        let operation = self
            .operation(OperationKind::UploadPack, cancellation)
            .await?;
        let maximum = usize::try_from(operation.max_logical_objects()).unwrap_or(usize::MAX);
        let capacity = object_ids.len().min(maximum);
        let mut unique = Vec::with_capacity(capacity);
        let mut seen = HashSet::with_capacity(capacity);
        let result = if object_ids.len() > capacity {
            Err(Error::LimitExceeded {
                limit: "pack object count",
                actual: object_ids.len() as u64,
                maximum: operation.max_logical_objects(),
            })
        } else {
            let mut result = Ok(());
            for oid in object_ids {
                if seen.insert(*oid) {
                    if unique.len() >= maximum {
                        result = Err(Error::LimitExceeded {
                            limit: "pack object count",
                            actual: unique.len().saturating_add(1) as u64,
                            maximum: operation.max_logical_objects(),
                        });
                        break;
                    }
                    unique.push(*oid);
                }
            }
            result.map(|()| unique)
        };
        let result = match result {
            Ok(unique) => generate_pack_with_operation(&operation, &unique, cancellation).await,
            Err(error) => Err(error),
        };
        operation.finish(result).await
    }
}

async fn generate_pack_with_operation(
    operation: &crate::OperationContext,
    object_ids: &[ObjectId],
    cancellation: &CancellationToken,
) -> Result<GeneratedPack> {
    let object_count = u32::try_from(object_ids.len()).map_err(|_| Error::LimitExceeded {
        limit: "pack object count",
        actual: object_ids.len() as u64,
        maximum: u32::MAX as u64,
    })?;
    let mut writer = PackWriter::new(object_count, operation.max_response_bytes())?;
    for batch in object_ids.chunks(OBJECT_BATCH_SIZE) {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let objects = operation.read_objects(batch).await?;
        let batch_cancellation = cancellation.clone();
        writer =
            tokio::task::spawn_blocking(move || writer.write_objects(objects, &batch_cancellation))
                .await
                .map_err(|source| Error::DecodeTask { source })??;
    }

    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let finish_cancellation = cancellation.clone();
    let pack = tokio::task::spawn_blocking(move || writer.finish(&finish_cancellation))
        .await
        .map_err(|source| Error::DecodeTask { source })??;
    let size = pack.size;
    operation
        .charge(BudgetDimension::ResponseBytes, size)
        .await?;
    Ok(pack)
}

struct PackWriter {
    file: NamedTempFile,
    hash: Sha1,
    object_count: u32,
    max_bytes: u64,
}

impl PackWriter {
    fn new(object_count: u32, max_bytes: u64) -> Result<Self> {
        if max_bytes < 20 {
            return Err(Error::LimitExceeded {
                limit: "pack response bytes",
                actual: 20,
                maximum: max_bytes,
            });
        }
        let mut file = NamedTempFile::new().map_err(io_error)?;
        let mut hash = Sha1::new();
        {
            let mut sink = HashingWriter {
                file: file.as_file_mut(),
                hash: &mut hash,
                written: 0,
                max_bytes: Some(max_bytes - 20),
            };
            sink.write_all(b"PACK").map_err(io_error)?;
            sink.write_all(&2u32.to_be_bytes()).map_err(io_error)?;
            sink.write_all(&object_count.to_be_bytes())
                .map_err(io_error)?;
        }
        Ok(Self {
            file,
            hash,
            object_count,
            max_bytes,
        })
    }

    fn write_objects(
        mut self,
        objects: Vec<RemoteGitObject>,
        cancellation: &CancellationToken,
    ) -> Result<Self> {
        let written = self.file.stream_position().map_err(io_error)?;
        let mut sink = HashingWriter {
            file: self.file.as_file_mut(),
            hash: &mut self.hash,
            written,
            max_bytes: Some(self.max_bytes - 20),
        };
        for object in objects {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            write_object(&mut sink, &object).map_err(|error| {
                if error.kind() == io::ErrorKind::FileTooLarge {
                    Error::LimitExceeded {
                        limit: "pack response bytes",
                        actual: self.max_bytes.saturating_add(1),
                        maximum: self.max_bytes,
                    }
                } else {
                    io_error(error)
                }
            })?;
        }
        Ok(self)
    }

    fn finish(mut self, cancellation: &CancellationToken) -> Result<GeneratedPack> {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let checksum: [u8; 20] = self.hash.finalize().into();
        let body_size = self.file.as_file().metadata().map_err(io_error)?.len();
        self.file
            .as_file_mut()
            .write_all(&checksum)
            .and_then(|_| self.file.as_file_mut().flush())
            .map_err(io_error)?;
        let size = self.file.as_file().metadata().map_err(io_error)?.len();
        if size > self.max_bytes || body_size.saturating_add(20) > self.max_bytes {
            return Err(Error::LimitExceeded {
                limit: "pack response bytes",
                actual: size.max(body_size.saturating_add(20)),
                maximum: self.max_bytes,
            });
        }
        self.file
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(io_error)?;
        let pack = GeneratedPack {
            file: self.file,
            size,
            checksum,
            object_count: self.object_count,
        };
        pack.verify_checksum()?;
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(pack)
    }
}

fn write_object(sink: &mut HashingWriter<'_>, object: &RemoteGitObject) -> io::Result<()> {
    let type_code = match object.kind {
        gix_object::Kind::Commit => 1,
        gix_object::Kind::Tree => 2,
        gix_object::Kind::Blob => 3,
        gix_object::Kind::Tag => 4,
    };
    write_pack_header(sink, type_code, object.data.len() as u64)?;
    let mut encoder = ZlibEncoder::new(sink, Compression::default());
    encoder.write_all(&object.data)?;
    let _ = encoder.finish()?;
    Ok(())
}

fn write_pack_header(writer: &mut impl Write, type_code: u8, size: u64) -> io::Result<()> {
    let mut value = size;
    let mut byte = (value & 0x0f) as u8 | (type_code << 4);
    value >>= 4;
    if value != 0 {
        byte |= 0x80;
    }
    writer.write_all(&[byte])?;
    while value != 0 {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        writer.write_all(&[byte])?;
    }
    Ok(())
}

struct HashingWriter<'a> {
    file: &'a mut std::fs::File,
    hash: &'a mut Sha1,
    written: u64,
    max_bytes: Option<u64>,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.max_bytes.is_some_and(|maximum| {
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum.saturating_sub(self.written)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "generated pack exceeds its response limit",
            ));
        }
        self.file.write_all(bytes)?;
        self.hash.update(bytes);
        self.written = self.written.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    band: Option<u8>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let payload_len = bytes.len() + band.map_or(0, |_| 1);
    let length = payload_len + 4;
    if length > 0xffff {
        return Err(Error::LimitExceeded {
            limit: "packet-line bytes",
            actual: length as u64,
            maximum: 0xffff,
        });
    }
    write_all_cancellable(writer, format!("{length:04x}").as_bytes(), cancellation).await?;
    if let Some(band) = band {
        write_all_cancellable(writer, &[band], cancellation).await?;
    }
    write_all_cancellable(writer, bytes, cancellation).await?;
    Ok(())
}

async fn write_all_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Error::Cancelled),
        result = writer.write_all(bytes) => result.map_err(io_error),
    }
}

fn io_error(error: io::Error) -> Error {
    Error::Metadata(crab_metadata::error::MetadataError::Io { source: error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn pack_object_contains_raw_git_payload_without_loose_header() {
        let mut file = NamedTempFile::new().expect("temporary pack object");
        let mut hash = Sha1::new();
        {
            let mut sink = HashingWriter {
                file: file.as_file_mut(),
                hash: &mut hash,
                written: 0,
                max_bytes: None,
            };
            let object = RemoteGitObject {
                oid: ObjectId::from_hex(b"0000000000000000000000000000000000000000")
                    .expect("object ID"),
                kind: gix_object::Kind::Blob,
                data: Bytes::from_static(b"hello"),
            };
            write_object(&mut sink, &object).expect("write pack object");
        }
        file.as_file_mut().flush().expect("flush pack object");

        let bytes = std::fs::read(file.path()).expect("read pack object");
        assert_eq!(
            bytes[0], 0x35,
            "blob header must encode a five-byte payload"
        );
        let mut decoder = flate2::read::ZlibDecoder::new(&bytes[1..]);
        let mut payload = Vec::new();
        decoder
            .read_to_end(&mut payload)
            .expect("decode pack object");
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn pack_header_uses_seven_bit_continuation_groups() {
        let mut header = Vec::new();
        write_pack_header(&mut header, 3, 4096).expect("write large pack header");
        assert_eq!(header, [0xb0, 0x80, 0x02]);
    }

    #[test]
    fn pack_writer_rejects_response_limit_before_writing_an_unbounded_temp_pack() {
        let writer = PackWriter::new(1, 32).expect("pack header fits the response bound");
        let object = RemoteGitObject {
            oid: ObjectId::from_hex(b"0000000000000000000000000000000000000000")
                .expect("object ID"),
            kind: gix_object::Kind::Blob,
            data: Bytes::from(vec![b'x'; 128]),
        };
        let result = writer.write_objects(vec![object], &CancellationToken::new());
        assert!(matches!(result, Err(Error::LimitExceeded { .. })));
    }

    #[test]
    fn generation_batch_covers_the_default_operation_object_bound() {
        let maximum = usize::try_from(crate::OperationLimits::default().max_logical_objects)
            .expect("default logical-object bound fits usize");

        assert!(OBJECT_BATCH_SIZE >= maximum);
    }
}
