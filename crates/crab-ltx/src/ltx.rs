// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.

use crate::error::{CrabError, Result};
use crate::{CHECKSUM_FLAG, Checksum, Txid};

pub const MAGIC: &[u8; 4] = b"LTX1";
pub const VERSION: i32 = 3;
pub const HEADER_SIZE: usize = 100;
pub const PAGE_HEADER_SIZE: usize = 6;
pub const TRAILER_SIZE: usize = 16;
pub const CHECKSUM_SIZE: usize = 8;

pub const HEADER_FLAG_NO_CHECKSUM: u32 = 1 << 1;
pub const HEADER_FLAG_MASK: u32 = HEADER_FLAG_NO_CHECKSUM;

pub const PAGE_HEADER_FLAG_SIZE: u16 = 1 << 0;
pub const PAGE_HEADER_FLAG_MASK: u16 = PAGE_HEADER_FLAG_SIZE;

pub const PENDING_BYTE: i64 = 0x4000_0000;

fn corrupt(msg: impl Into<String>) -> CrabError {
    // Wrap a format error as LTXCorrupted, matching litestream's classification
    // of malformed LTX content (litestream.go ErrLTXCorrupted).
    let _ = msg;
    CrabError::LTXCorrupted
}

pub fn lock_pgno(page_size: u32) -> u32 {
    if page_size == 0 {
        return 0;
    }
    (PENDING_BYTE / page_size as i64) as u32 + 1
}

#[derive(Clone)]
pub struct Crc64 {
    digest: crc_fast::Digest,
}

impl Default for Crc64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc64 {
    pub fn new() -> Self {
        Self {
            digest: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc64GoIso),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.digest.update(data);
    }

    pub fn sum64(&self) -> u64 {
        self.digest.finalize()
    }
}

pub fn checksum_page(pgno: u32, data: &[u8]) -> Checksum {
    let mut h = Crc64::new();
    h.update(&pgno.to_be_bytes());
    h.update(data);
    CHECKSUM_FLAG | h.sum64()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub version: i32,
    pub flags: u32,
    pub page_size: u32,
    pub commit: u32,
    pub min_txid: Txid,
    pub max_txid: Txid,
    pub timestamp: i64,
    pub pre_apply_checksum: Checksum,
    pub wal_offset: i64,
    pub wal_size: i64,
    pub wal_salt1: u32,
    pub wal_salt2: u32,
    pub node_id: u64,
}

impl Header {
    pub fn is_snapshot(&self) -> bool {
        self.min_txid == Txid(1)
    }

    pub fn no_checksum(&self) -> bool {
        self.flags & HEADER_FLAG_NO_CHECKSUM != 0
    }

    pub fn parse(b: &[u8]) -> Result<Header> {
        if b.len() < HEADER_SIZE {
            return Err(corrupt("short header"));
        }
        if &b[0..4] != MAGIC {
            return Err(corrupt("bad magic"));
        }
        Ok(Header {
            version: VERSION,
            flags: u32_be(&b[4..]),
            page_size: u32_be(&b[8..]),
            commit: u32_be(&b[12..]),
            min_txid: Txid(u64_be(&b[16..])),
            max_txid: Txid(u64_be(&b[24..])),
            timestamp: u64_be(&b[32..]) as i64,
            pre_apply_checksum: u64_be(&b[40..]),
            wal_offset: u64_be(&b[48..]) as i64,
            wal_size: u64_be(&b[56..]) as i64,
            wal_salt1: u32_be(&b[64..]),
            wal_salt2: u32_be(&b[68..]),
            node_id: u64_be(&b[72..]),
        })
    }

    pub fn marshal(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..4].copy_from_slice(MAGIC);
        b[4..8].copy_from_slice(&self.flags.to_be_bytes());
        b[8..12].copy_from_slice(&self.page_size.to_be_bytes());
        b[12..16].copy_from_slice(&self.commit.to_be_bytes());
        b[16..24].copy_from_slice(&self.min_txid.0.to_be_bytes());
        b[24..32].copy_from_slice(&self.max_txid.0.to_be_bytes());
        b[32..40].copy_from_slice(&(self.timestamp as u64).to_be_bytes());
        b[40..48].copy_from_slice(&self.pre_apply_checksum.to_be_bytes());
        b[48..56].copy_from_slice(&(self.wal_offset as u64).to_be_bytes());
        b[56..64].copy_from_slice(&(self.wal_size as u64).to_be_bytes());
        b[64..68].copy_from_slice(&self.wal_salt1.to_be_bytes());
        b[68..72].copy_from_slice(&self.wal_salt2.to_be_bytes());
        b[72..80].copy_from_slice(&self.node_id.to_be_bytes());
        b
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            return Err(corrupt("invalid version"));
        }
        if self.flags != (self.flags & HEADER_FLAG_MASK) {
            return Err(corrupt("invalid flags"));
        }
        if !is_valid_page_size(self.page_size) {
            return Err(corrupt("invalid page size"));
        }
        if self.min_txid == Txid(0) {
            return Err(corrupt("minimum transaction id required"));
        }
        if self.max_txid == Txid(0) {
            return Err(corrupt("maximum transaction id required"));
        }
        if self.min_txid > self.max_txid {
            return Err(corrupt("transaction ids out of order"));
        }
        if self.wal_offset < 0 {
            return Err(corrupt("wal offset cannot be negative"));
        }
        if self.wal_size < 0 {
            return Err(corrupt("wal size cannot be negative"));
        }
        if (self.wal_salt1 != 0 || self.wal_salt2 != 0) && self.wal_offset == 0 {
            return Err(corrupt("wal offset required if salt exists"));
        }
        if self.wal_offset == 0 && self.wal_size != 0 {
            return Err(corrupt("wal offset required if wal size exists"));
        }
        if self.is_snapshot() {
            if self.pre_apply_checksum != 0 {
                return Err(corrupt("pre-apply checksum must be zero on snapshots"));
            }
        } else if self.no_checksum() {
            if self.pre_apply_checksum != 0 {
                return Err(corrupt("pre-apply checksum not allowed"));
            }
        } else {
            if self.pre_apply_checksum == 0 {
                return Err(corrupt("pre-apply checksum required on non-snapshot files"));
            }
            if self.pre_apply_checksum & CHECKSUM_FLAG == 0 {
                return Err(corrupt("invalid pre-apply checksum format"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageHeader {
    pub pgno: u32,
    pub flags: u16,
}

impl PageHeader {
    pub fn is_zero(&self) -> bool {
        self.pgno == 0 && self.flags == 0
    }

    pub fn parse(b: &[u8]) -> Result<PageHeader> {
        if b.len() < PAGE_HEADER_SIZE {
            return Err(corrupt("short page header"));
        }
        Ok(PageHeader {
            pgno: u32_be(&b[0..]),
            flags: u16_be(&b[4..]),
        })
    }

    pub fn marshal(&self) -> [u8; PAGE_HEADER_SIZE] {
        let mut b = [0u8; PAGE_HEADER_SIZE];
        b[0..4].copy_from_slice(&self.pgno.to_be_bytes());
        b[4..6].copy_from_slice(&self.flags.to_be_bytes());
        b
    }

    pub fn validate(&self) -> Result<()> {
        if self.pgno == 0 {
            return Err(corrupt("page number required"));
        }
        if self.flags != (self.flags & PAGE_HEADER_FLAG_MASK) {
            return Err(corrupt("invalid page header flags"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trailer {
    pub post_apply_checksum: Checksum,
    pub file_checksum: Checksum,
}

impl Trailer {
    pub fn validate(&self, header: Header) -> Result<()> {
        if header.no_checksum() {
            if self.post_apply_checksum != 0 {
                return Err(corrupt("post-apply checksum not allowed"));
            }
        } else if self.post_apply_checksum == 0 || self.post_apply_checksum & CHECKSUM_FLAG == 0 {
            return Err(corrupt("invalid post-apply checksum"));
        }

        if self.file_checksum == 0 || self.file_checksum & CHECKSUM_FLAG == 0 {
            return Err(corrupt("invalid file checksum"));
        }
        Ok(())
    }

    pub fn parse(b: &[u8]) -> Result<Trailer> {
        if b.len() < TRAILER_SIZE {
            return Err(corrupt("short trailer"));
        }
        Ok(Trailer {
            post_apply_checksum: u64_be(&b[0..]),
            file_checksum: u64_be(&b[8..]),
        })
    }

    pub fn marshal(&self) -> [u8; TRAILER_SIZE] {
        let mut b = [0u8; TRAILER_SIZE];
        b[0..8].copy_from_slice(&self.post_apply_checksum.to_be_bytes());
        b[8..16].copy_from_slice(&self.file_checksum.to_be_bytes());
        b
    }
}

pub fn is_valid_page_size(sz: u32) -> bool {
    let mut i = 512u32;
    while i <= 65536 {
        if sz == i {
            return true;
        }
        i *= 2;
    }
    false
}

#[derive(Debug, Clone)]
pub struct DecodedFile {
    pub header: Header,
    pub trailer: Trailer,
}

type DecodedPages = Vec<(u32, Vec<u8>)>;

pub fn decode_file(bytes: &[u8]) -> Result<DecodedFile> {
    decode_file_inner(bytes, false).map(|(file, _)| file)
}

pub(crate) fn inspect_reader(reader: impl std::io::Read) -> Result<(DecodedFile, u64, [u8; 32])> {
    let (file, _, size, digest) = decode_reader_inner(reader, false)?;
    Ok((file, size, digest))
}

pub(crate) fn decode_file_with_pages(bytes: &[u8]) -> Result<(DecodedFile, DecodedPages)> {
    decode_file_inner(bytes, true)
}

fn decode_file_inner(bytes: &[u8], retain_pages: bool) -> Result<(DecodedFile, DecodedPages)> {
    let (file, pages, _, _) = decode_reader_inner(std::io::Cursor::new(bytes), retain_pages)?;
    Ok((file, pages))
}

fn decode_reader_inner(
    reader: impl std::io::Read,
    retain_pages: bool,
) -> Result<(DecodedFile, DecodedPages, u64, [u8; 32])> {
    let mut decoder = crate::codec::Decoder::new(reader);
    decoder.decode_header()?;
    let header = decoder.header;
    let mut pages = Vec::new();
    let mut data = vec![0; header.page_size as usize];

    while let Some(page) = decoder.decode_page(&mut data)? {
        if retain_pages {
            pages.push((page.pgno, data.clone()));
        }
    }
    decoder.close()?;

    let (size, digest) = decoder.artifact()?;

    Ok((
        DecodedFile {
            header,
            trailer: decoder.trailer,
        },
        pages,
        size,
        digest,
    ))
}

pub fn encode_file(
    header: &Header,
    pages: &[(u32, Vec<u8>)],
    post_apply_checksum: Checksum,
) -> Result<Vec<u8>> {
    encode_file_inner(header, pages, post_apply_checksum)
}

fn encode_file_inner(
    header: &Header,
    pages: &[(u32, Vec<u8>)],
    post_apply_checksum: Checksum,
) -> Result<Vec<u8>> {
    let mut encoder = crate::codec::Encoder::new_block(Vec::new());
    encoder.encode_header(*header)?;
    for (page_number, data) in pages {
        encoder.encode_page(
            PageHeader {
                pgno: *page_number,
                flags: 0,
            },
            data,
        )?;
    }
    encoder.close(post_apply_checksum)?;
    Ok(encoder.writer)
}

fn u16_be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn u32_be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn u64_be(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}
