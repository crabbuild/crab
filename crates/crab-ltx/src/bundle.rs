//! Celld-inspired verbatim LTX envelopes with bounded, checksum-verified rows.
//! Apache-2.0; adapted from bundle.rs at the revision in UPSTREAM.md.

use crate::{CrabError, Limits, Result, SegmentInfo};
use bytes::Bytes;
use crab_storage::{MultipartUploadSource, StorageError};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::TempPath;

/// Keeps a server-owned verified bundle artifact alive while an overlay uses it.
pub trait BundleLease: Send + Sync {}

/// One immutable segment and its repository/epoch identity, before bundling.
pub struct BundleEntry {
    pub repository: String,
    pub epoch: String,
    pub info: SegmentInfo,
    pub bytes: Vec<u8>,
}

impl BundleEntry {
    /// Creates an entry using the canonical identity selected by `CellReplica`.
    #[must_use]
    pub fn for_cell(
        cell: [u8; 32],
        incarnation: [u8; 16],
        info: SegmentInfo,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            repository: encode_identity(&cell),
            epoch: encode_identity(&incarnation),
            info,
            bytes,
        }
    }
}

/// A verified segment's byte extent in a bundle; identity is not authorization.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleRow {
    pub repository: String,
    pub epoch: String,
    pub info: SegmentInfo,
    pub offset: u64,
}

enum BundleBody {
    Memory(Bytes),
    File(Arc<BundleFile>),
}

struct BundleFile {
    path: PathBuf,
    remove_on_drop: bool,
}

impl Drop for BundleFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Clone for BundleBody {
    fn clone(&self) -> Self {
        match self {
            Self::Memory(bytes) => Self::Memory(bytes.clone()),
            Self::File(file) => Self::File(Arc::clone(file)),
        }
    }
}

/// An owned, validated envelope containing verbatim checksum-bearing LTX files.
///
/// The format is Crab's `CRB1`, not Celld's `CLB1`: rows retain string epochs,
/// exact ranges and whole-file checksums. A manifest must pin its locations.
pub struct Bundle {
    body: BundleBody,
    rows: Vec<BundleRow>,
    digest: [u8; 32],
    length: u64,
}

/// File-backed builder for a verified bundle.
///
/// Segment bodies are written as they arrive; only the bounded row table is
/// retained in memory. `finish` seals the footer and revalidates the complete
/// envelope before returning the owned temporary bundle.
pub struct BundleBuilder {
    path: TempPath,
    rows: Vec<BundleRow>,
    identities: BTreeSet<(String, String, u64, u64)>,
    payload_len: u64,
    limits: Limits,
    poisoned: bool,
}

impl BundleBuilder {
    /// Creates a private temporary bundle in an existing runtime scratch dir.
    pub fn new_temp(directory: &Path, limits: Limits) -> Result<Self> {
        let limits = limits.validate()?;
        let file = tempfile::Builder::new()
            .prefix(".crab-bundle-")
            .tempfile_in(directory)?;
        Ok(Self {
            path: file.into_temp_path(),
            rows: Vec::new(),
            identities: BTreeSet::new(),
            payload_len: 0,
            limits,
            poisoned: false,
        })
    }

    /// Appends one fully verified segment without retaining its body.
    pub fn push(&mut self, entry: BundleEntry) -> Result<()> {
        if self.poisoned {
            return Err(CrabError::InvalidState("bundle builder is poisoned"));
        }
        if self.rows.len() >= self.limits.max_segments
            || entry.repository.is_empty()
            || entry.repository.len() > 4096
            || !valid_epoch(&entry.epoch)
        {
            return Err(CrabError::Limit("bundle entries"));
        }
        crate::recovery::verify_segment(&entry.bytes, &entry.info, self.limits)?;
        let identity = (
            entry.repository.clone(),
            entry.epoch.clone(),
            entry.info.min_txid,
            entry.info.max_txid,
        );
        if !self.identities.insert(identity) {
            return Err(CrabError::LTXCorrupted);
        }
        let next_len = self
            .payload_len
            .checked_add(entry.info.size_bytes)
            .ok_or(CrabError::Limit("bundle bytes"))?;
        if next_len > self.limits.max_plan_bytes {
            return Err(CrabError::Limit("bundle bytes"));
        }
        let write_result = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(&entry.bytes));
        if let Err(error) = write_result {
            self.poisoned = true;
            return Err(error.into());
        }
        self.rows.push(BundleRow {
            repository: entry.repository,
            epoch: entry.epoch,
            info: entry.info,
            offset: self.payload_len,
        });
        self.payload_len = next_len;
        Ok(())
    }

    /// Seals, syncs, validates, and transfers ownership of the bundle file.
    pub fn finish(self) -> Result<Bundle> {
        if self.poisoned || self.rows.is_empty() {
            return Err(CrabError::InvalidState("bundle builder has no valid rows"));
        }
        let footer = serde_json::to_vec(&self.rows)?;
        let footer_len =
            u32::try_from(footer.len()).map_err(|_| CrabError::Limit("bundle footer"))?;
        let total = self
            .payload_len
            .checked_add(footer.len() as u64)
            .and_then(|length| length.checked_add(8))
            .ok_or(CrabError::Limit("bundle bytes"))?;
        if total > self.limits.max_plan_bytes {
            return Err(CrabError::Limit("bundle bytes"));
        }
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(&footer)?;
        file.write_all(&footer_len.to_le_bytes())?;
        file.write_all(b"CRB1")?;
        file.sync_all()?;
        Bundle::decode_temp_file(self.path, self.limits)
    }
}

impl Bundle {
    /// Encodes bounded segments; duplicate identities and malformed LTX are rejected.
    pub fn encode(entries: Vec<BundleEntry>, limits: Limits) -> Result<Self> {
        let limits = limits.validate()?;
        if entries.is_empty() || entries.len() > limits.max_segments {
            return Err(CrabError::Limit("bundle entries"));
        }
        let mut bytes = Vec::new();
        let mut rows = Vec::new();
        for entry in entries {
            if entry.repository.is_empty()
                || entry.repository.len() > 4096
                || !valid_epoch(&entry.epoch)
            {
                return Err(CrabError::LTXCorrupted);
            }
            crate::recovery::verify_segment(&entry.bytes, &entry.info, limits)?;
            if (bytes.len() as u64).saturating_add(entry.info.size_bytes) > limits.max_plan_bytes {
                return Err(CrabError::Limit("bundle bytes"));
            }
            rows.push(BundleRow {
                repository: entry.repository,
                epoch: entry.epoch,
                info: entry.info,
                offset: bytes.len() as u64,
            });
            bytes.extend(entry.bytes);
        }
        let footer = serde_json::to_vec(&rows)?;
        let len = u32::try_from(footer.len()).map_err(|_| CrabError::Limit("bundle footer"))?;
        bytes.extend(footer);
        bytes.extend(len.to_le_bytes());
        bytes.extend(b"CRB1");
        Self::decode(bytes, limits)
    }

    /// Verifies the complete envelope, every extent, and every segment digest.
    pub fn decode(bytes: Vec<u8>, limits: Limits) -> Result<Self> {
        Self::decode_bytes(bytes.into(), limits)
    }

    /// Verifies a shared envelope without copying its complete body.
    pub fn decode_bytes(bytes: Bytes, limits: Limits) -> Result<Self> {
        let limits = limits.validate()?;
        if bytes.len() as u64 > limits.max_plan_bytes {
            return Err(CrabError::Limit("bundle bytes"));
        }
        let (rows, payload_end) = decode_footer(&bytes, limits)?;
        validate_row_layout(&rows, payload_end, limits)?;
        for row in &rows {
            let start = usize::try_from(row.offset).map_err(|_| CrabError::LTXCorrupted)?;
            let end = start
                .checked_add(
                    usize::try_from(row.info.size_bytes).map_err(|_| CrabError::LTXCorrupted)?,
                )
                .ok_or(CrabError::LTXCorrupted)?;
            crate::recovery::verify_segment(&bytes[start..end], &row.info, limits)?;
        }
        Ok(Self {
            digest: *blake3::hash(&bytes).as_bytes(),
            length: bytes.len() as u64,
            body: BundleBody::Memory(bytes),
            rows,
        })
    }

    /// Verifies an envelope from a caller-owned file without retaining its body in memory.
    pub fn decode_file(path: &Path, limits: Limits) -> Result<Self> {
        let (rows, length, digest) = decode_file_contents(path, limits)?;
        Ok(Self {
            body: BundleBody::File(Arc::new(BundleFile {
                path: path.to_owned(),
                remove_on_drop: false,
            })),
            rows,
            digest,
            length,
        })
    }

    /// Verifies a temporary envelope and removes its file when the bundle is dropped.
    pub fn decode_temp_file(path: TempPath, limits: Limits) -> Result<Self> {
        let source = path.to_path_buf();
        let (rows, length, digest) = decode_file_contents(&source, limits)?;
        let source = path
            .keep()
            .map_err(|error| CrabError::Other(Box::new(error)))?;
        Ok(Self {
            body: BundleBody::File(Arc::new(BundleFile {
                path: source,
                remove_on_drop: true,
            })),
            rows,
            digest,
            length,
        })
    }

    /// Verifies the expected outer digest before parsing a temporary envelope.
    pub fn decode_temp_file_with_digest(
        path: TempPath,
        expected: [u8; 32],
        limits: Limits,
    ) -> Result<Self> {
        let source = path.to_path_buf();
        let length = std::fs::metadata(&source)?.len();
        let limits = limits.validate()?;
        if length > limits.max_plan_bytes {
            return Err(CrabError::Limit("bundle bytes"));
        }
        let digest = hash_file(&source)?;
        if digest != expected {
            return Err(CrabError::ChecksumMismatch);
        }
        let (rows, verified_length) = decode_file_rows(&source, limits)?;
        if verified_length != length {
            return Err(CrabError::LTXCorrupted);
        }
        let source = path
            .keep()
            .map_err(|error| CrabError::Other(Box::new(error)))?;
        Ok(Self {
            body: BundleBody::File(Arc::new(BundleFile {
                path: source,
                remove_on_drop: true,
            })),
            rows,
            digest,
            length,
        })
    }

    /// Detaches a uniquely owned temporary bundle file from this bundle.
    ///
    /// The caller assumes responsibility for the returned path. Memory-backed
    /// bundles and bundles still referenced by an upload source cannot detach.
    pub fn detach_file(self) -> Result<PathBuf> {
        let Bundle { body, .. } = self;
        let BundleBody::File(file) = body else {
            return Err(CrabError::InvalidState(
                "memory-backed bundle has no detachable file",
            ));
        };
        let mut file = Arc::try_unwrap(file)
            .map_err(|_| CrabError::InvalidState("bundle file is still referenced"))?;
        file.remove_on_drop = false;
        Ok(std::mem::take(&mut file.path))
    }

    #[must_use]
    pub fn rows(&self) -> &[BundleRow] {
        &self.rows
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.length
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    #[must_use]
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Returns the complete verified body, allocating only for file-backed bundles.
    pub fn read_all(&self) -> Result<Bytes> {
        match &self.body {
            BundleBody::Memory(bytes) => Ok(bytes.clone()),
            BundleBody::File(file) => {
                let bytes = std::fs::read(&file.path)?;
                if bytes.len() as u64 != self.length
                    || *blake3::hash(&bytes).as_bytes() != self.digest
                {
                    return Err(CrabError::ChecksumMismatch);
                }
                Ok(Bytes::from(bytes))
            }
        }
    }

    #[must_use = "use or handle the verified bundle bytes"]
    pub fn bytes(&self) -> Result<Bytes> {
        self.read_all()
    }

    /// Returns a re-openable multipart source for the verified envelope.
    pub fn upload_source(&self) -> Arc<dyn MultipartUploadSource> {
        Arc::new(BundleUploadSource {
            body: self.body.clone(),
            length: self.length,
        })
    }

    /// Returns the verified bytes for a row index, never a caller-supplied extent.
    pub fn read_segment(&self, index: usize) -> Result<Bytes> {
        let row = self.rows.get(index).ok_or(CrabError::TxNotAvailable)?;
        let start = usize::try_from(row.offset).map_err(|_| CrabError::LTXCorrupted)?;
        let length = usize::try_from(row.info.size_bytes).map_err(|_| CrabError::LTXCorrupted)?;
        match &self.body {
            BundleBody::Memory(bytes) => Ok(bytes.slice(start..start + length)),
            BundleBody::File(file) => {
                let mut source = File::open(&file.path)?;
                let mut bytes = vec![0; length];
                read_exact_at(&mut source, row.offset, &mut bytes)?;
                if *blake3::hash(&bytes).as_bytes() != row.info.blake3 {
                    return Err(CrabError::ChecksumMismatch);
                }
                Ok(Bytes::from(bytes))
            }
        }
    }
}

struct BundleUploadSource {
    body: BundleBody,
    length: u64,
}

#[async_trait::async_trait]
impl MultipartUploadSource for BundleUploadSource {
    async fn byte_len(&self) -> crab_storage::Result<u64> {
        match &self.body {
            BundleBody::Memory(_) => Ok(self.length),
            BundleBody::File(file) => Ok(tokio::fs::metadata(&file.path).await?.len()),
        }
    }

    async fn read_exact(&self, offset: u64, length: usize) -> crab_storage::Result<Bytes> {
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| StorageError::ReadRejected {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "bundle upload range overflows",
                )),
            })?;
        if end > self.length {
            return Err(StorageError::ReadRejected {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "bundle upload range exceeds verified body",
                )),
            });
        }
        match &self.body {
            BundleBody::Memory(bytes) => {
                let start = usize::try_from(offset).map_err(|_| StorageError::ReadRejected {
                    source: Box::new(std::io::Error::other("bundle upload offset overflows")),
                })?;
                Ok(bytes.slice(start..start + length))
            }
            BundleBody::File(file) => {
                let path = file.path.clone();
                tokio::task::spawn_blocking(move || {
                    let mut source = File::open(path)?;
                    let mut bytes = vec![0; length];
                    read_exact_at(&mut source, offset, &mut bytes)?;
                    Ok::<_, std::io::Error>(Bytes::from(bytes))
                })
                .await
                .map_err(|error| StorageError::ReadRejected {
                    source: Box::new(error),
                })?
                .map_err(Into::into)
            }
        }
    }
}

fn decode_footer(bytes: &[u8], limits: Limits) -> Result<(Vec<BundleRow>, u64)> {
    let trailer = bytes.len().checked_sub(8).ok_or(CrabError::LTXCorrupted)?;
    if &bytes[trailer + 4..] != b"CRB1" {
        return Err(CrabError::LTXCorrupted);
    }
    let length = u32::from_le_bytes(
        bytes[trailer..trailer + 4]
            .try_into()
            .map_err(|_| CrabError::LTXCorrupted)?,
    ) as usize;
    let start = trailer.checked_sub(length).ok_or(CrabError::LTXCorrupted)?;
    let rows: Vec<BundleRow> = serde_json::from_slice(&bytes[start..trailer])?;
    if rows.is_empty() || rows.len() > limits.max_segments {
        return Err(CrabError::Limit("bundle entries"));
    }
    Ok((rows, start as u64))
}

fn decode_file_contents(path: &Path, limits: Limits) -> Result<(Vec<BundleRow>, u64, [u8; 32])> {
    let limits = limits.validate()?;
    let (rows, length) = decode_file_rows(path, limits)?;
    let digest = hash_file(path)?;
    Ok((rows, length, digest))
}

fn decode_file_rows(path: &Path, limits: Limits) -> Result<(Vec<BundleRow>, u64)> {
    let mut source = File::open(path)?;
    let length = source.metadata()?.len();
    if length > limits.max_plan_bytes {
        return Err(CrabError::Limit("bundle bytes"));
    }
    let trailer_offset = length.checked_sub(8).ok_or(CrabError::LTXCorrupted)?;
    let mut trailer = [0; 8];
    read_exact_at(&mut source, trailer_offset, &mut trailer)?;
    if &trailer[4..] != b"CRB1" {
        return Err(CrabError::LTXCorrupted);
    }
    let footer_length = u32::from_le_bytes(
        trailer[..4]
            .try_into()
            .map_err(|_| CrabError::LTXCorrupted)?,
    ) as u64;
    let footer_start = trailer_offset
        .checked_sub(footer_length)
        .ok_or(CrabError::LTXCorrupted)?;
    let footer_size =
        usize::try_from(footer_length).map_err(|_| CrabError::Limit("bundle footer"))?;
    let mut footer = vec![0; footer_size];
    read_exact_at(&mut source, footer_start, &mut footer)?;
    let rows: Vec<BundleRow> = serde_json::from_slice(&footer)?;
    validate_row_layout(&rows, footer_start, limits)?;
    for row in &rows {
        let size = usize::try_from(row.info.size_bytes).map_err(|_| CrabError::LTXCorrupted)?;
        let mut bytes = vec![0; size];
        read_exact_at(&mut source, row.offset, &mut bytes)?;
        crate::recovery::verify_segment(&bytes, &row.info, limits)?;
    }
    Ok((rows, length))
}

fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut source = File::open(path)?;
    let mut digest = blake3::Hasher::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(*digest.finalize().as_bytes())
}

fn validate_row_layout(rows: &[BundleRow], payload_end: u64, limits: Limits) -> Result<()> {
    if rows.is_empty() || rows.len() > limits.max_segments {
        return Err(CrabError::Limit("bundle entries"));
    }
    let mut end = 0u64;
    let mut identities = BTreeSet::new();
    for row in rows {
        if row.repository.is_empty()
            || row.repository.len() > 4096
            || !valid_epoch(&row.epoch)
            || row.offset != end
            || !identities.insert((
                &row.repository,
                &row.epoch,
                row.info.min_txid,
                row.info.max_txid,
            ))
        {
            return Err(CrabError::LTXCorrupted);
        }
        end = end
            .checked_add(row.info.size_bytes)
            .ok_or(CrabError::LTXCorrupted)?;
        if end > payload_end {
            return Err(CrabError::LTXCorrupted);
        }
    }
    if end != payload_end {
        return Err(CrabError::LTXCorrupted);
    }
    Ok(())
}

fn read_exact_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(bytes)
}

pub(crate) fn cell_identity(cell: &[u8; 32], incarnation: &[u8; 16]) -> (String, String) {
    (encode_identity(cell), encode_identity(incarnation))
}

fn encode_identity(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn valid_epoch(epoch: &str) -> bool {
    !epoch.is_empty()
        && epoch.len() <= 128
        && epoch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}
