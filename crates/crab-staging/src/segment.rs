//! Segment file writer and reader (length-framed + CRC32C).
//!
//! Record format: `[len: LE u32][data: len bytes][crc32c: LE u32]`.
//! No file-level header, no magic. Segments are append-only; reads use
//! `pread` and never disturb the writer's offset.
//!
//! CRC32C is computed over `data` only (not over `len`). Hardware-
//! accelerated on x86-64 (SSE 4.2) and aarch64 (CRC32 extension).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, IoSlice, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tracing::debug;

use crate::error::{Result, StagingError};

/// Per-record framing overhead: 4 bytes for `len` + 4 bytes for `crc32c`.
const FRAMING_OVERHEAD: u32 = 8;
const RECORD_IO_SLICE_LIMIT: usize = 96;

// --- Record encoding / decoding ---

/// Encode a chunk into the on-disk record format.
///
/// Returns `[len: LE u32][data][crc32c(data): LE u32]`.
#[must_use]
pub fn encode_record(data: &[u8]) -> Vec<u8> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "caller ensures data.len() fits in u32 (bounded by segment hard cap)"
    )]
    let len = data.len() as u32;
    let crc = crc32c::crc32c(data);
    let mut buf = Vec::with_capacity(data.len() + FRAMING_OVERHEAD as usize);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(data);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

/// Decode and verify a record from a buffer that was read at a known offset.
///
/// `buf` must contain exactly `4 + expected_len + 4` bytes. Validates that
/// the stored length matches `expected_len` and that the CRC32C over the
/// data region is correct. Returns the data as `Bytes`.
///
/// # Errors
///
/// Returns [`StagingError::CrcMismatch`] if the stored length does not
/// match `expected_len` or if the CRC32C check fails.
pub fn decode_record(buf: &[u8], expected_len: u32, segment_id: u64, offset: u64) -> Result<Bytes> {
    verify_record(buf, expected_len, segment_id, offset)?;
    Ok(Bytes::copy_from_slice(&buf[4..4 + expected_len as usize]))
}

fn verify_record(buf: &[u8], expected_len: u32, segment_id: u64, offset: u64) -> Result<()> {
    let total = 4 + expected_len as usize + 4;
    if buf.len() < total {
        return Err(StagingError::CrcMismatch { segment_id, offset });
    }

    let stored_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if stored_len != expected_len {
        return Err(StagingError::CrcMismatch { segment_id, offset });
    }

    let data = &buf[4..4 + expected_len as usize];
    let crc_start = 4 + expected_len as usize;
    let stored_crc = u32::from_le_bytes([
        buf[crc_start],
        buf[crc_start + 1],
        buf[crc_start + 2],
        buf[crc_start + 3],
    ]);

    let computed_crc = crc32c::crc32c(data);
    if computed_crc != stored_crc {
        return Err(StagingError::CrcMismatch { segment_id, offset });
    }

    Ok(())
}

// --- ChunkLocator ---

/// Location of a chunk's bytes within a segment file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLocator {
    /// Segment containing this chunk.
    pub segment_id: u64,
    /// Byte offset of the record start (the `len` field) in the segment.
    pub offset: u64,
    /// Data-byte count, excluding the 8 bytes of framing.
    pub length: u32,
}

pub(crate) struct PreparedRecord<'a> {
    data: &'a [u8],
    len_bytes: [u8; 4],
    crc_bytes: [u8; 4],
    data_len: u32,
    record_len: u64,
}

impl<'a> PreparedRecord<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        let data_len: u32 = data
            .len()
            .try_into()
            .map_err(|_| StagingError::Configuration {
                key: format!("chunk size {} exceeds u32::MAX", data.len()),
                origin: "staging".into(),
            })?;
        let record_len = u64::from(data_len) + u64::from(FRAMING_OVERHEAD);
        let crc = crc32c::crc32c(data);
        Ok(Self {
            data,
            len_bytes: data_len.to_le_bytes(),
            crc_bytes: crc.to_le_bytes(),
            data_len,
            record_len,
        })
    }

    pub(crate) fn record_len(&self) -> u64 {
        self.record_len
    }
}

// --- SegmentWriter ---

/// Writes chunk records to the current segment file.
///
/// Held behind `tokio::sync::Mutex` in `StagingArea`. The lock covers
/// only the synchronous write + offset bump; async work (fsync, `SQLite`)
/// happens outside the critical section.
pub struct SegmentWriter {
    pub(crate) segment_id: u64,
    pub(crate) file: File,
    pub(crate) write_offset: u64,
    pub(crate) pending_bytes: u64,
    pub(crate) soft_cap: u64,
    pub(crate) hard_cap: u64,
}

impl SegmentWriter {
    /// Open or create `segments/current.seg` with `O_APPEND`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] on filesystem failure.
    pub fn new(segments_dir: &Path, segment_id: u64, soft_cap: u64, hard_cap: u64) -> Result<Self> {
        fs::create_dir_all(segments_dir)?;
        let path = segments_dir.join("current.seg");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let write_offset = file.metadata()?.len();
        Ok(Self {
            segment_id,
            file,
            write_offset,
            pending_bytes: 0,
            soft_cap,
            hard_cap,
        })
    }

    /// Re-open `segments/current.seg` after crash recovery with a known
    /// write offset (the truncated size).
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] on filesystem failure.
    pub fn open_recovered(
        segments_dir: &Path,
        segment_id: u64,
        write_offset: u64,
        soft_cap: u64,
        hard_cap: u64,
    ) -> Result<Self> {
        fs::create_dir_all(segments_dir)?;
        let path = segments_dir.join("current.seg");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            segment_id,
            file,
            write_offset,
            pending_bytes: 0,
            soft_cap,
            hard_cap,
        })
    }

    /// Append a chunk record and return its locator.
    ///
    /// The returned offset points at the `len` field; `length` is the
    /// data-byte count (excluding framing). No fsync is performed.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Configuration`] if the record would exceed
    /// the hard cap, or [`StagingError::Io`] on write failure.
    #[cfg(test)]
    pub fn append(&mut self, data: &[u8]) -> Result<ChunkLocator> {
        let record = PreparedRecord::new(data)?;
        let locators = self.append_prepared(std::slice::from_ref(&record))?;
        if let [locator] = locators.as_slice() {
            return Ok(*locator);
        }

        Err(StagingError::Internal(
            "single segment append did not return exactly one locator".to_owned(),
        ))
    }

    pub(crate) fn append_prepared(
        &mut self,
        records: &[PreparedRecord<'_>],
    ) -> Result<Vec<ChunkLocator>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }

        let total_record_len = records.iter().try_fold(0u64, |acc, record| {
            acc.checked_add(record.record_len()).ok_or_else(|| {
                StagingError::StagingCorrupt("segment append byte count overflow".to_owned())
            })
        })?;
        let next_offset = self
            .write_offset
            .checked_add(total_record_len)
            .ok_or_else(|| StagingError::StagingCorrupt("segment offset overflow".to_owned()))?;

        if next_offset > self.hard_cap {
            return Err(StagingError::Configuration {
                key: format!(
                    "record batch of {} bytes would exceed segment hard cap ({} bytes)",
                    total_record_len, self.hard_cap
                ),
                origin: "staging".into(),
            });
        }

        let mut offset = self.write_offset;
        let mut locators = Vec::with_capacity(records.len());
        for record in records {
            locators.push(ChunkLocator {
                segment_id: self.segment_id,
                offset,
                length: record.data_len,
            });
            offset = offset
                .checked_add(record.record_len())
                .ok_or_else(|| StagingError::StagingCorrupt("segment offset overflow".into()))?;
        }

        write_records_vectored(&mut self.file, records)?;
        self.write_offset = next_offset;
        self.pending_bytes = self
            .pending_bytes
            .checked_add(total_record_len)
            .ok_or_else(|| StagingError::StagingCorrupt("pending byte count overflow".into()))?;

        Ok(locators)
    }

    pub(crate) fn append_prepared_until_soft_cap(
        &mut self,
        records: &[PreparedRecord<'_>],
    ) -> Result<Vec<ChunkLocator>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }

        let mut end = 1usize;
        let mut projected_offset = self
            .write_offset
            .checked_add(records[0].record_len())
            .ok_or_else(|| StagingError::StagingCorrupt("segment offset overflow".to_owned()))?;
        while end < records.len() && projected_offset < self.soft_cap {
            projected_offset = projected_offset
                .checked_add(records[end].record_len())
                .ok_or_else(|| StagingError::StagingCorrupt("segment offset overflow".into()))?;
            end += 1;
        }

        self.append_prepared(&records[..end])
    }

    /// Whether the current segment has reached its soft cap and should
    /// be sealed.
    #[must_use]
    pub fn should_seal(&self) -> bool {
        self.write_offset >= self.soft_cap
    }

    /// Seal the current segment: fsync, rename to its final name, and
    /// fsync the parent directory.
    ///
    /// After this call the writer is consumed and must not be reused.
    /// Intended to be called from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] on filesystem failure.
    pub fn seal(self, segments_dir: &Path) -> Result<()> {
        self.file.sync_data()?;
        drop(self.file);

        let current = segments_dir.join("current.seg");
        let sealed_name = format!("{:016x}.seg", self.segment_id);
        let sealed_path = segments_dir.join(&sealed_name);
        fs::rename(&current, &sealed_path)?;

        // fsync the parent directory to make the rename durable.
        let dir_fd = File::open(segments_dir)?;
        dir_fd.sync_all()?;

        debug!(
            segment_id = self.segment_id,
            bytes = self.write_offset,
            "sealed segment"
        );

        Ok(())
    }

    /// Current write offset (byte position of the next append).
    #[must_use]
    pub fn write_offset(&self) -> u64 {
        self.write_offset
    }

    /// Segment id of the current segment.
    #[must_use]
    pub fn segment_id(&self) -> u64 {
        self.segment_id
    }

    /// Bytes written since the last flush boundary.
    #[must_use]
    pub fn pending_bytes(&self) -> u64 {
        self.pending_bytes
    }

    /// Reset the pending-bytes counter (called after a flush boundary).
    pub fn reset_pending(&mut self) {
        self.pending_bytes = 0;
    }
}

// --- SegmentReader ---

/// Read-only handle to a sealed (or current) segment file.
///
/// Wraps `Arc<File>` for thread-safe `pread` access. No seek, no
/// mutation of the file.
pub struct SegmentReader {
    segment_id: u64,
    file: Arc<File>,
}

impl SegmentReader {
    /// Open a segment file for reading.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] if the file cannot be opened.
    #[cfg(test)]
    pub fn open(path: &Path, segment_id: u64) -> Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            segment_id,
            file: Arc::new(file),
        })
    }

    /// Read and verify a single record at the given offset.
    ///
    /// Performs a single `pread` of `4 + length + 4` bytes, validates
    /// the stored length and CRC32C, and returns the data bytes.
    ///
    /// Thread-safe: uses `pread` (no seek), safe to call from multiple
    /// threads concurrently on the same `SegmentReader`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::CrcMismatch`] if the stored length does
    /// not match `length` or if the CRC32C check fails.
    /// Returns [`StagingError::Io`] on read failure.
    pub fn read(&self, offset: u64, length: u32) -> Result<Bytes> {
        let total = 4 + length as usize + 4;
        let mut buf = vec![0u8; total];
        read_exact_at(&self.file, &mut buf, offset).map_err(|e| {
            if e.kind() == ErrorKind::UnexpectedEof {
                StagingError::CrcMismatch {
                    segment_id: self.segment_id,
                    offset,
                }
            } else {
                StagingError::Io(e)
            }
        })?;
        decode_record(&buf, length, self.segment_id, offset)
    }

    /// Read sorted records, coalescing adjacent offsets into range reads.
    ///
    /// Records must be ordered by segment offset and use `(input_index,
    /// offset, data_length)`. Returned bytes preserve each input index.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::CrcMismatch`] if any record is truncated or
    /// has invalid framing/CRC, or [`StagingError::Io`] on read failure.
    pub(crate) fn read_many_sorted(
        &self,
        records: &[(usize, u64, u32)],
    ) -> Result<Vec<(usize, Bytes)>> {
        let mut results = Vec::with_capacity(records.len());
        let mut run_start = 0usize;
        while run_start < records.len() {
            let mut run_end = run_start + 1;
            let (_, start_offset, start_len) = records[run_start];
            let mut end_offset = record_end_offset(start_offset, start_len)?;
            while run_end < records.len() {
                let (_, next_offset, next_len) = records[run_end];
                if next_offset != end_offset {
                    break;
                }
                end_offset = record_end_offset(next_offset, next_len)?;
                run_end += 1;
            }

            let span_len =
                usize::try_from(end_offset.checked_sub(start_offset).ok_or_else(|| {
                    StagingError::StagingCorrupt(
                        "segment record offsets are not ordered".to_owned(),
                    )
                })?)
                .map_err(|_| {
                    StagingError::StagingCorrupt("segment read span is too large".into())
                })?;
            let mut buf = vec![0u8; span_len];
            read_exact_at(&self.file, &mut buf, start_offset).map_err(|e| {
                if e.kind() == ErrorKind::UnexpectedEof {
                    StagingError::CrcMismatch {
                        segment_id: self.segment_id,
                        offset: start_offset,
                    }
                } else {
                    StagingError::Io(e)
                }
            })?;
            let bytes = Bytes::from(buf);

            for &(input_index, offset, length) in &records[run_start..run_end] {
                let relative =
                    usize::try_from(offset.checked_sub(start_offset).ok_or_else(|| {
                        StagingError::StagingCorrupt(
                            "segment record offsets are not ordered".to_owned(),
                        )
                    })?)
                    .map_err(|_| {
                        StagingError::StagingCorrupt("segment record offset is too large".into())
                    })?;
                let total = 4 + length as usize + 4;
                let record = bytes.get(relative..relative + total).ok_or_else(|| {
                    StagingError::CrcMismatch {
                        segment_id: self.segment_id,
                        offset,
                    }
                })?;
                verify_record(record, length, self.segment_id, offset)?;
                results.push((
                    input_index,
                    bytes.slice(relative + 4..relative + 4 + length as usize),
                ));
            }

            run_start = run_end;
        }

        Ok(results)
    }

    /// The segment id this reader is bound to.
    #[must_use]
    #[cfg(test)]
    pub fn segment_id(&self) -> u64 {
        self.segment_id
    }
}

fn record_end_offset(offset: u64, length: u32) -> Result<u64> {
    offset
        .checked_add(u64::from(length) + u64::from(FRAMING_OVERHEAD))
        .ok_or_else(|| StagingError::StagingCorrupt("segment record offset overflow".to_owned()))
}

fn write_records_vectored(file: &mut File, records: &[PreparedRecord<'_>]) -> std::io::Result<()> {
    let mut record_idx = 0usize;
    let mut part_idx = 0u8;
    let mut part_offset = 0usize;

    while record_idx < records.len() {
        let mut bufs = Vec::with_capacity(RECORD_IO_SLICE_LIMIT);
        let mut idx = record_idx;
        let mut part = part_idx;
        let mut offset = part_offset;

        while idx < records.len() && bufs.len() < RECORD_IO_SLICE_LIMIT {
            let slice = record_part_slice(&records[idx], part);
            if offset < slice.len() {
                bufs.push(IoSlice::new(&slice[offset..]));
            }
            part += 1;
            offset = 0;
            if part == 3 {
                idx += 1;
                part = 0;
            }
        }

        match file.write_vectored(&bufs) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "failed to write segment records",
                ));
            }
            Ok(written) => advance_record_position(
                records,
                &mut record_idx,
                &mut part_idx,
                &mut part_offset,
                written,
            ),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

fn record_part_slice<'record, 'data: 'record>(
    record: &'record PreparedRecord<'data>,
    part: u8,
) -> &'record [u8] {
    match part {
        0 => &record.len_bytes,
        1 => record.data,
        _ => &record.crc_bytes,
    }
}

fn advance_record_position(
    records: &[PreparedRecord<'_>],
    record_idx: &mut usize,
    part_idx: &mut u8,
    part_offset: &mut usize,
    mut written: usize,
) {
    while written > 0 && *record_idx < records.len() {
        let slice = record_part_slice(&records[*record_idx], *part_idx);
        let remaining = slice.len().saturating_sub(*part_offset);
        if remaining == 0 {
            *part_idx += 1;
            *part_offset = 0;
            if *part_idx == 3 {
                *record_idx += 1;
                *part_idx = 0;
            }
            continue;
        }

        let consumed = written.min(remaining);
        *part_offset += consumed;
        written -= consumed;
        if *part_offset == slice.len() {
            *part_idx += 1;
            *part_offset = 0;
            if *part_idx == 3 {
                *record_idx += 1;
                *part_idx = 0;
            }
        }
    }
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    file.seek_read(buf, offset)
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match read_at(file, buf, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "failed to fill segment read buffer",
                ));
            }
            Ok(n) => {
                offset += n as u64;
                let (_, rest) = buf.split_at_mut(n);
                buf = rest;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// --- ReaderPool (LRU) ---

/// LRU pool of open segment file descriptors for reads.
///
/// Bookkeeping `Mutex` is never held across `.await`.
pub struct ReaderPool {
    root: PathBuf,
    fd_pool_size: usize,
    /// LRU order: most-recently-used at the back.
    inner: Mutex<PoolInner>,
}

struct PoolInner {
    map: HashMap<u64, Arc<File>>,
    /// LRU order: most-recently-used at the back of the vec.
    order: Vec<u64>,
}

impl ReaderPool {
    /// Create a new reader pool rooted at `segments_dir`.
    #[must_use]
    pub fn new(segments_dir: PathBuf, fd_pool_size: usize) -> Self {
        Self {
            root: segments_dir,
            fd_pool_size,
            inner: Mutex::new(PoolInner {
                map: HashMap::new(),
                order: Vec::new(),
            }),
        }
    }

    /// Get a reader for the given segment, opening the file if not cached.
    ///
    /// The bookkeeping lock is held only for the `HashMap` lookup and LRU
    /// update — never across `.await` or I/O.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] if the segment file cannot be opened.
    pub fn get(&self, segment_id: u64) -> Result<SegmentReader> {
        let file = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|e| StagingError::Internal(format!("reader pool lock poisoned: {e}")))?;

            if let Some(f) = inner.map.get(&segment_id).cloned() {
                // Promote to most-recently-used.
                inner.order.retain(|&id| id != segment_id);
                inner.order.push(segment_id);
                f
            } else {
                // Not cached — we'll open outside the lock, then insert.
                // But first, evict if at capacity.
                while inner.map.len() >= self.fd_pool_size {
                    if let Some(evict_id) = inner.order.first().copied() {
                        inner.order.remove(0);
                        inner.map.remove(&evict_id);
                    } else {
                        break;
                    }
                }

                // Open the file (this is I/O but the lock is still held;
                // acceptable because open() is fast and we avoid a race
                // where two threads open the same segment).
                let path = self.segment_path(segment_id);
                let file = Arc::new(File::open(&path)?);
                inner.map.insert(segment_id, Arc::clone(&file));
                inner.order.push(segment_id);
                file
            }
        };

        Ok(SegmentReader { segment_id, file })
    }

    fn segment_path(&self, segment_id: u64) -> PathBuf {
        // Try the sealed name first, fall back to current.seg.
        let sealed = self.root.join(format!("{segment_id:016x}.seg"));
        if sealed.exists() {
            sealed
        } else {
            self.root.join("current.seg")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let data = b"hello, segment world!";
        let encoded = encode_record(data);
        let decoded = decode_record(&encoded, data.len() as u32, 1, 0)
            .expect("round-trip decode should succeed");
        assert_eq!(&decoded[..], data);
    }

    #[test]
    fn encode_decode_empty_data() {
        let data: &[u8] = b"";
        let encoded = encode_record(data);
        assert_eq!(encoded.len(), 8); // 4 + 0 + 4
        let decoded =
            decode_record(&encoded, 0, 1, 0).expect("empty data round-trip should succeed");
        assert!(decoded.is_empty());
    }

    #[test]
    fn tampered_data_byte_returns_crc_mismatch() {
        let data = b"original data bytes";
        let mut encoded = encode_record(data);
        // Flip a bit in the data region.
        encoded[6] ^= 0x01;
        let err = decode_record(&encoded, data.len() as u32, 42, 100)
            .expect_err("tampered data should fail CRC");
        assert!(matches!(
            err,
            StagingError::CrcMismatch {
                segment_id: 42,
                offset: 100
            }
        ));
    }

    #[test]
    fn tampered_len_returns_crc_mismatch_without_reading_past_boundary() {
        let data = b"some chunk data";
        let mut encoded = encode_record(data);
        // Corrupt the stored length to a larger value.
        let bad_len: u32 = data.len() as u32 + 999;
        encoded[0..4].copy_from_slice(&bad_len.to_le_bytes());
        // Decode with the *original* expected_len — stored_len != expected_len.
        let err = decode_record(&encoded, data.len() as u32, 7, 0)
            .expect_err("mismatched len should fail");
        assert!(matches!(
            err,
            StagingError::CrcMismatch {
                segment_id: 7,
                offset: 0
            }
        ));
    }

    #[test]
    fn tampered_crc32c_field_returns_crc_mismatch() {
        let data = b"crc tamper test";
        let mut encoded = encode_record(data);
        // Flip a bit in the trailing CRC field.
        let crc_offset = 4 + data.len();
        encoded[crc_offset] ^= 0xFF;
        let err =
            decode_record(&encoded, data.len() as u32, 1, 0).expect_err("tampered CRC should fail");
        assert!(matches!(err, StagingError::CrcMismatch { .. }));
    }

    #[test]
    fn truncated_segment_missing_crc_returns_crc_mismatch() {
        let data = b"truncation test data";
        let encoded = encode_record(data);
        // Chop off the last 2 bytes of the CRC trailer.
        let truncated = &encoded[..encoded.len() - 2];
        let err = decode_record(truncated, data.len() as u32, 5, 0)
            .expect_err("truncated record should fail");
        assert!(matches!(
            err,
            StagingError::CrcMismatch {
                segment_id: 5,
                offset: 0
            }
        ));
    }

    #[test]
    fn multiple_records_have_non_overlapping_well_ordered_offsets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        let mut writer = SegmentWriter::new(&segments_dir, 1, 1 << 30, 1 << 30).expect("writer");

        let chunks: Vec<Vec<u8>> = vec![
            vec![0xAA; 100],
            vec![0xBB; 200],
            vec![0xCC; 50],
            vec![0xDD; 300],
        ];

        let mut locators = Vec::new();
        for chunk in &chunks {
            locators.push(writer.append(chunk).expect("append"));
        }

        // Offsets must be strictly increasing.
        for pair in locators.windows(2) {
            assert!(
                pair[0].offset < pair[1].offset,
                "offsets must be strictly increasing"
            );
        }

        // Records must not overlap: each record occupies
        // [offset, offset + 4 + length + 4).
        for pair in locators.windows(2) {
            let end_of_first = pair[0].offset + 4 + u64::from(pair[0].length) + 4;
            assert!(end_of_first <= pair[1].offset, "records must not overlap");
        }

        // Verify each locator's length matches the original data.
        for (loc, chunk) in locators.iter().zip(&chunks) {
            assert_eq!(loc.length as usize, chunk.len());
        }
    }

    #[test]
    fn writer_append_and_reader_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        let mut writer = SegmentWriter::new(&segments_dir, 1, 1 << 30, 1 << 30).expect("writer");

        let data = b"round-trip through writer and reader";
        let loc = writer.append(data).expect("append");

        // Read back via SegmentReader on the same current.seg.
        let reader = SegmentReader::open(&segments_dir.join("current.seg"), 1).expect("reader");
        let got = reader.read(loc.offset, loc.length).expect("read");
        assert_eq!(&got[..], data);
    }

    #[test]
    fn writer_append_preserves_record_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        let mut writer = SegmentWriter::new(&segments_dir, 1, 1 << 30, 1 << 30).expect("writer");

        let data = b"record layout bytes";
        writer.append(data).expect("append");
        drop(writer);

        let raw = std::fs::read(segments_dir.join("current.seg")).expect("read segment");
        assert_eq!(raw, encode_record(data));
    }

    #[test]
    fn writer_append_prepared_preserves_record_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        let mut writer = SegmentWriter::new(&segments_dir, 1, 1 << 30, 1 << 30).expect("writer");

        let chunks: Vec<&[u8]> = vec![b"first".as_slice(), b"second chunk".as_slice(), b""];
        let records: Vec<PreparedRecord<'_>> = chunks
            .iter()
            .map(|data| PreparedRecord::new(data).expect("prepare"))
            .collect();
        let locators = writer.append_prepared(&records).expect("append prepared");
        drop(writer);

        let mut expected = Vec::new();
        for chunk in &chunks {
            expected.extend_from_slice(&encode_record(chunk));
        }
        let raw = std::fs::read(segments_dir.join("current.seg")).expect("read segment");

        assert_eq!(locators.len(), chunks.len());
        assert_eq!(raw, expected);
    }

    #[test]
    fn reader_pool_caches_and_evicts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        std::fs::create_dir_all(&segments_dir).expect("mkdir");

        // Create two sealed segment files with one record each.
        for seg_id in 1..=3u64 {
            let mut w =
                SegmentWriter::new(&segments_dir, seg_id, 1 << 30, 1 << 30).expect("writer");
            w.append(&[seg_id as u8; 16]).expect("append");
            w.seal(&segments_dir).expect("seal");
        }

        // Pool with capacity 2 — third access should evict the oldest.
        let pool = ReaderPool::new(segments_dir, 2);

        let r1 = pool.get(1).expect("get seg 1");
        assert_eq!(r1.segment_id(), 1);

        let r2 = pool.get(2).expect("get seg 2");
        assert_eq!(r2.segment_id(), 2);

        // This should evict segment 1.
        let r3 = pool.get(3).expect("get seg 3");
        assert_eq!(r3.segment_id(), 3);

        // Segment 1 was evicted but can be re-opened.
        let r1_again = pool.get(1).expect("get seg 1 again");
        assert_eq!(r1_again.segment_id(), 1);
    }

    #[test]
    fn seal_renames_current_to_id_based_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        let mut writer = SegmentWriter::new(&segments_dir, 42, 1 << 30, 1 << 30).expect("writer");
        writer.append(b"seal test").expect("append");
        writer.seal(&segments_dir).expect("seal");

        assert!(!segments_dir.join("current.seg").exists());
        assert!(segments_dir.join("000000000000002a.seg").exists());
    }

    #[test]
    fn append_exceeding_hard_cap_returns_configuration_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        // Hard cap of 20 bytes — a 100-byte chunk + 8 framing = 108 > 20.
        let mut writer = SegmentWriter::new(&segments_dir, 1, 10, 20).expect("writer");
        let err = writer
            .append(&[0u8; 100])
            .expect_err("should exceed hard cap");
        assert!(matches!(err, StagingError::Configuration { .. }));
    }
}
