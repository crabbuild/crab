// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.

use std::collections::{HashMap, HashSet};

use crate::{WAL_FRAME_HEADER_SIZE, WAL_HEADER_SIZE, wal_checksum};

const WAL_VERSION: u32 = 3_007_000;

const WAL_MAGIC_LITTLE_ENDIAN: u32 = 0x377f_0682;

const WAL_MAGIC_BIG_ENDIAN: u32 = 0x377f_0683;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalError {
    Eof,

    InvalidMagic(u32),
    InvalidPageSize(u32),

    UnsupportedVersion(u32),

    BufferSize { got: usize, want: u32 },

    OffsetTooSmall { offset: i64, header_size: i64 },

    UnalignedOffset { offset: i64, page_size: u32 },

    PrevFrameMismatch,
}

impl WalError {
    #[inline]
    pub fn is_eof(&self) -> bool {
        matches!(self, WalError::Eof)
    }
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Matches the Go string "EOF" (io.EOF.CrabError()).
            WalError::Eof => f.write_str("EOF"),
            // Go: fmt.Errorf("invalid wal header magic: %x", magic) — lowercase
            // hex, no leading zeros (matches Go's %x for a uint32).
            WalError::InvalidPageSize(size) => write!(f, "invalid WAL page size: {size}"),
            WalError::InvalidMagic(magic) => write!(f, "invalid wal header magic: {magic:x}"),
            WalError::UnsupportedVersion(v) => write!(f, "unsupported wal version: {v}"),
            WalError::BufferSize { got, want } => write!(
                f,
                "WALReader.ReadFrame(): buffer size ({got}) must match page size ({want})"
            ),
            WalError::OffsetTooSmall {
                offset,
                header_size,
            } => write!(
                f,
                "offset ({offset}) must be greater than the wal header size ({header_size})"
            ),
            WalError::UnalignedOffset { offset, page_size } => {
                write!(f, "unaligned wal offset {offset} for page size {page_size}")
            }
            WalError::PrevFrameMismatch => f.write_str("previous frame mismatch"),
        }
    }
}

impl std::error::Error for WalError {}

impl From<WalError> for crate::CrabError {
    fn from(e: WalError) -> Self {
        crate::CrabError::Other(Box::new(e))
    }
}

type WalResult<T> = std::result::Result<T, WalError>;

#[derive(Debug)]
pub struct WalReader<'a> {
    data: &'a [u8],
    tail_base: usize,
    frame_n: i64,

    big_endian: bool,
    page_size: u32,
    salt1: u32,
    salt2: u32,
    chksum1: u32,
    chksum2: u32,
}

impl<'a> WalReader<'a> {
    pub fn new(data: &'a [u8]) -> WalResult<Self> {
        let mut r = WalReader {
            data,
            tail_base: 0,
            frame_n: 0,
            big_endian: false,
            page_size: 0,
            salt1: 0,
            salt2: 0,
            chksum1: 0,
            chksum2: 0,
        };
        r.read_header()?;
        Ok(r)
    }

    pub fn new_with_offset(data: &'a [u8], offset: i64, salt1: u32, salt2: u32) -> WalResult<Self> {
        // Must not start on the first page — we need to read the previous frame.
        if offset <= WAL_HEADER_SIZE as i64 {
            return Err(WalError::OffsetTooSmall {
                offset,
                header_size: WAL_HEADER_SIZE as i64,
            });
        }
        let mut r = Self::new_with_offset_inner(data, 0)?;
        r.seek_to_offset(offset, salt1, salt2)?;
        Ok(r)
    }

    fn new_with_offset_inner(data: &'a [u8], tail_base: usize) -> WalResult<Self> {
        let mut r = WalReader {
            data,
            tail_base,
            frame_n: 0,
            big_endian: false,
            page_size: 0,
            salt1: 0,
            salt2: 0,
            chksum1: 0,
            chksum2: 0,
        };
        // Read header to determine page size & byte order.
        r.read_header()?;
        Ok(r)
    }

    fn seek_to_offset(&mut self, offset: i64, salt1: u32, salt2: u32) -> WalResult<()> {
        self.salt1 = salt1;
        self.salt2 = salt2;

        let frame_size = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        if (offset - WAL_HEADER_SIZE as i64) % frame_size != 0 {
            return Err(WalError::UnalignedOffset {
                offset,
                page_size: self.page_size,
            });
        }
        self.frame_n = (offset - WAL_HEADER_SIZE as i64) / frame_size;

        // Read the previous frame to load the running checksum. Any failure here
        // (salt/checksum mismatch surfaces as WalError::Eof from read_frame_inner)
        // means the previous frame doesn't match what we expect → mismatch.
        self.frame_n -= 1;
        let mut buf = vec![0u8; self.page_size as usize];
        if self.read_frame_inner(&mut buf, false).is_err() {
            return Err(WalError::PrevFrameMismatch);
        }
        Ok(())
    }

    #[inline]
    pub fn salt(&self) -> (u32, u32) {
        (self.salt1, self.salt2)
    }

    pub fn offset(&self) -> i64 {
        if self.frame_n == 0 {
            return 0;
        }
        WAL_HEADER_SIZE as i64
            + ((self.frame_n - 1) * (WAL_FRAME_HEADER_SIZE as i64 + self.page_size as i64))
    }

    fn read_at(&self, offset: i64, n: usize) -> Option<&'a [u8]> {
        if offset < 0 {
            return None;
        }
        let offset = offset as usize;
        let start = if self.tail_base == 0 || offset < WAL_HEADER_SIZE {
            offset
        } else {
            // A tail image holds nothing between the header and the tail;
            // an offset in that gap is a short read, exactly as a whole-file
            // image shorter than the offset would be.
            WAL_HEADER_SIZE + offset.checked_sub(self.tail_base)?
        };
        let end = start.checked_add(n)?;
        if end > self.data.len() {
            return None;
        }
        Some(&self.data[start..end])
    }

    pub fn new_with_offset_over_tail(
        data: &'a [u8],
        tail_base: i64,
        offset: i64,
        salt1: u32,
        salt2: u32,
    ) -> WalResult<Self> {
        if tail_base < WAL_HEADER_SIZE as i64 || offset < tail_base {
            return Err(WalError::OffsetTooSmall {
                offset,
                header_size: WAL_HEADER_SIZE as i64,
            });
        }
        let mut r = Self::new_with_offset_inner(data, tail_base as usize)?;
        r.seek_to_offset(offset, salt1, salt2)?;
        Ok(r)
    }

    fn read_header(&mut self) -> WalResult<()> {
        // If we have a partial WAL, mark WAL as done (io.EOF).
        let hdr = match self.read_at(0, WAL_HEADER_SIZE) {
            Some(b) => b,
            None => return Err(WalError::Eof),
        };

        // Determine byte order of checksums from the magic (always read
        // big-endian, like Go's binary.BigEndian.Uint32(hdr[0:])).
        let magic = be_u32(&hdr[0..]);
        self.big_endian = match magic {
            WAL_MAGIC_LITTLE_ENDIAN => false,
            WAL_MAGIC_BIG_ENDIAN => true,
            _ => return Err(WalError::InvalidMagic(magic)),
        };

        // If the header checksum doesn't match then we may have failed with a
        // partial WAL header write during checkpointing => io.EOF.
        let chksum1 = be_u32(&hdr[24..]);
        let chksum2 = be_u32(&hdr[28..]);
        let (v0, v1) = wal_checksum(self.big_endian, 0, 0, &hdr[..24]);
        if v0 != chksum1 || v1 != chksum2 {
            return Err(WalError::Eof);
        }

        // Verify version is correct.
        let version = be_u32(&hdr[4..]);
        if version != WAL_VERSION {
            return Err(WalError::UnsupportedVersion(version));
        }

        self.page_size = be_u32(&hdr[8..]);
        if !crate::ltx::is_valid_page_size(self.page_size) {
            return Err(WalError::InvalidPageSize(self.page_size));
        }
        self.salt1 = be_u32(&hdr[16..]);
        self.salt2 = be_u32(&hdr[20..]);
        self.chksum1 = chksum1;
        self.chksum2 = chksum2;

        Ok(())
    }

    pub fn read_frame(&mut self, data: &mut [u8]) -> WalResult<(u32, u32)> {
        self.read_frame_inner(data, true)
    }

    fn read_frame_inner(
        &mut self,
        data: &mut [u8],
        verify_checksum: bool,
    ) -> WalResult<(u32, u32)> {
        if data.len() != self.page_size as usize {
            return Err(WalError::BufferSize {
                got: data.len(),
                want: self.page_size,
            });
        }

        let frame_size = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        let offset = WAL_HEADER_SIZE as i64 + (self.frame_n * frame_size);

        // Read WAL frame header. A short read is io.EOF.
        let hdr = match self.read_at(offset, WAL_FRAME_HEADER_SIZE) {
            Some(b) => b,
            None => return Err(WalError::Eof),
        };

        // Read WAL page data. A short read is io.EOF.
        let page = match self.read_at(offset + WAL_FRAME_HEADER_SIZE as i64, data.len()) {
            Some(b) => b,
            None => return Err(WalError::Eof),
        };
        data.copy_from_slice(page);

        // Verify salt matches the salt in the header; otherwise end of valid WAL.
        let salt1 = be_u32(&hdr[8..]);
        let salt2 = be_u32(&hdr[12..]);
        if self.salt1 != salt1 || self.salt2 != salt2 {
            return Err(WalError::Eof);
        }

        // Verify the cumulative checksum. If verification is disabled, it is
        // because we are jumping to an offset and not checksumming from the
        // beginning, so we simply adopt the frame's stored checksum.
        let chksum1 = be_u32(&hdr[16..]);
        let chksum2 = be_u32(&hdr[20..]);
        if verify_checksum {
            let (c0, c1) = wal_checksum(self.big_endian, self.chksum1, self.chksum2, &hdr[..8]);
            let (c0, c1) = wal_checksum(self.big_endian, c0, c1, data);
            self.chksum1 = c0;
            self.chksum2 = c1;
            if self.chksum1 != chksum1 || self.chksum2 != chksum2 {
                return Err(WalError::Eof);
            }
        } else {
            self.chksum1 = chksum1;
            self.chksum2 = chksum2;
        }

        let pgno = be_u32(&hdr[0..]);
        let commit = be_u32(&hdr[4..]);

        self.frame_n += 1;

        Ok((pgno, commit))
    }

    pub fn page_map(&mut self) -> WalResult<(HashMap<u32, i64>, i64, u32)> {
        let mut m: HashMap<u32, i64> = HashMap::new();
        let mut tx_map: HashMap<u32, i64> = HashMap::new();
        let mut commit: u32 = 0;
        let mut data = vec![0u8; self.page_size as usize];

        loop {
            let (pgno, fcommit) = match self.read_frame(&mut data) {
                Ok(v) => v,
                Err(e) if e.is_eof() => break,
                Err(e) => return Err(e),
            };

            // Update latest offset for this page within the current transaction.
            // Not promoted to the full map until the txn commits.
            let offset = self.offset();
            tx_map.insert(pgno, offset);

            // On a commit record, transfer the txn offsets into the full map and
            // record the new DB size.
            if fcommit != 0 {
                for (p, o) in tx_map.drain() {
                    m.insert(p, o);
                }
                commit = fcommit;
            }
        }

        // Remove pages that exceed the final commit size (DB shrank mid-WAL).
        m.retain(|&pgno, _| pgno <= commit);

        // No complete transactions => original (zero) offset.
        if m.is_empty() {
            return Ok((m, 0, 0));
        }

        // Highest page offset, extended to the end of that frame.
        let mut end: i64 = 0;
        for &offset in m.values() {
            if end == 0 || offset > end {
                end = offset;
            }
        }
        end += WAL_FRAME_HEADER_SIZE as i64 + self.page_size as i64;

        Ok((m, end, commit))
    }

    pub fn frame_salts_until(&self, until: (u32, u32)) -> HashSet<(u32, u32)> {
        let mut m = HashSet::new();
        let step = WAL_FRAME_HEADER_SIZE as i64 + self.page_size as i64;
        let mut offset = WAL_HEADER_SIZE as i64;
        // The loop ends either when a frame-header read runs short (the Go
        // `n != len(hdr)` => break) or when we reach the `until` salt below.
        while let Some(hdr) = self.read_at(offset, WAL_FRAME_HEADER_SIZE) {
            let salt1 = be_u32(&hdr[8..]);
            let salt2 = be_u32(&hdr[12..]);

            // Track unique salts.
            m.insert((salt1, salt2));

            // Stop once we've seen the salt we were asked to read up to.
            if salt1 == until.0 && salt2 == until.1 {
                break;
            }

            offset += step;
        }
        m
    }
}

#[inline]
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
