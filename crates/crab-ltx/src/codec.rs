// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.

use crate::CHECKSUM_FLAG;
use crate::error::{CrabError, Result};
use crate::ltx::{
    CHECKSUM_SIZE, Crc64, HEADER_SIZE, Header, PAGE_HEADER_FLAG_SIZE, PAGE_HEADER_SIZE, PageHeader,
    TRAILER_SIZE, Trailer, checksum_page, lock_pgno,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderState {
    Header,
    Pages,
    Close,
    Closed,
}

pub(crate) struct Decoder<R> {
    reader: CountingReader<R>,
    index: Vec<(u32, u64, u64)>,
    state: DecoderState,
    pub(crate) header: Header,
    pub(crate) trailer: Trailer,
    hash: Crc64,
    rolling_checksum: u64,
}

impl<R: Read> Decoder<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader: CountingReader {
                inner: reader,
                bytes: 0,
            },
            index: Vec::new(),
            state: DecoderState::Header,
            header: Header::default(),
            trailer: Trailer::default(),
            hash: Crc64::new(),
            rolling_checksum: 0,
        }
    }

    pub(crate) fn decode_header(&mut self) -> Result<()> {
        if self.state != DecoderState::Header {
            return Err(CrabError::LTXCorrupted);
        }
        let mut bytes = [0; HEADER_SIZE];
        self.reader.read_exact(&mut bytes)?;
        self.header = Header::parse(&bytes)?;
        self.header.validate()?;
        self.hash.update(&bytes);
        if !self.header.no_checksum() {
            self.rolling_checksum = CHECKSUM_FLAG;
        }
        self.state = DecoderState::Pages;
        Ok(())
    }

    pub(crate) fn decode_page(&mut self, data: &mut [u8]) -> Result<Option<PageHeader>> {
        if self.state == DecoderState::Close {
            return Ok(None);
        }
        if self.state != DecoderState::Pages || data.len() != self.header.page_size as usize {
            return Err(CrabError::LTXCorrupted);
        }

        let offset = self.reader.bytes;
        let mut header_bytes = [0; PAGE_HEADER_SIZE];
        self.reader.read_exact(&mut header_bytes)?;
        let page = PageHeader::parse(&header_bytes)?;
        self.hash.update(&header_bytes);
        if page.is_zero() {
            self.state = DecoderState::Close;
            return Ok(None);
        }
        page.validate()?;
        let previous = self.index.last().map(|entry| entry.0).unwrap_or(0);
        let lock = lock_pgno(self.header.page_size);
        let expected = previous + if previous == lock - 1 { 2 } else { 1 };
        if page.pgno <= previous
            || page.pgno > self.header.commit
            || page.pgno == lock
            || (self.header.is_snapshot() && page.pgno != expected)
        {
            return Err(CrabError::LTXCorrupted);
        }

        if page.flags & PAGE_HEADER_FLAG_SIZE != 0 {
            let mut size_bytes = [0; 4];
            self.reader.read_exact(&mut size_bytes)?;
            self.hash.update(&size_bytes);
            let compressed_size = u32::from_be_bytes(size_bytes) as usize;
            if compressed_size > crate::lz4_block::compress_bound(data.len()) {
                return Err(CrabError::LTXCorrupted);
            }
            let mut compressed = vec![0; compressed_size];
            self.reader.read_exact(&mut compressed)?;
            let n = lz4_flex::block::decompress_into(&compressed, data)
                .map_err(|error| CrabError::Other(Box::new(error)))?;
            if n != data.len() {
                return Err(CrabError::LTXCorrupted);
            }
        } else {
            let mut decoder = lz4_flex::frame::FrameDecoder::new(&mut self.reader);
            decoder
                .read_exact(data)
                .map_err(|error| CrabError::Other(Box::new(error)))?;
            let mut extra = [0; 1];
            if decoder
                .read(&mut extra)
                .map_err(|error| CrabError::Other(Box::new(error)))?
                != 0
            {
                return Err(CrabError::LTXCorrupted);
            }
        }

        self.index
            .push((page.pgno, offset, self.reader.bytes - offset));
        self.hash.update(data);
        if self.header.is_snapshot()
            && !self.header.no_checksum()
            && page.pgno != lock_pgno(self.header.page_size)
        {
            self.rolling_checksum =
                CHECKSUM_FLAG | (self.rolling_checksum ^ checksum_page(page.pgno, data));
        }
        Ok(Some(page))
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        if self.state == DecoderState::Closed {
            return Ok(());
        }
        if self.state != DecoderState::Close {
            return Err(CrabError::LTXCorrupted);
        }

        let mut remaining = Vec::new();
        self.reader.read_to_end(&mut remaining)?;
        if remaining.len() < 8 + TRAILER_SIZE {
            return Err(CrabError::LTXCorrupted);
        }

        let trailer_offset = remaining.len() - TRAILER_SIZE;
        let size_offset = trailer_offset - 8;
        let index_size = u64::from_be_bytes(
            remaining[size_offset..trailer_offset]
                .try_into()
                .map_err(|_| CrabError::LTXCorrupted)?,
        ) as usize;
        if index_size != size_offset {
            return Err(CrabError::LTXCorrupted);
        }
        if decode_page_index(&remaining[..size_offset])? != self.index {
            return Err(CrabError::LTXCorrupted);
        }
        if self.header.is_snapshot() {
            let last = if self.header.commit == lock_pgno(self.header.page_size) {
                self.header.commit - 1
            } else {
                self.header.commit
            };
            if self.index.last().map(|entry| entry.0).unwrap_or(0) != last {
                return Err(CrabError::LTXCorrupted);
            }
        }

        self.trailer = Trailer::parse(&remaining[trailer_offset..])?;
        self.trailer.validate(self.header)?;
        self.hash
            .update(&remaining[..remaining.len() - CHECKSUM_SIZE]);
        if CHECKSUM_FLAG | self.hash.sum64() != self.trailer.file_checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        if self.header.is_snapshot()
            && !self.header.no_checksum()
            && self.rolling_checksum != self.trailer.post_apply_checksum
        {
            return Err(CrabError::ChecksumMismatch);
        }

        self.state = DecoderState::Closed;
        Ok(())
    }
}

pub(crate) struct Encoder<W> {
    pub(crate) writer: W,
    pub(crate) header: Header,
    pub(crate) trailer: Trailer,
    hash: Crc64,
    index: BTreeMap<u32, (u64, u64)>,
    compressor: crate::lz4_block::Compressor,
    bytes_written: u64,
    previous_page_number: u32,
    header_written: bool,
    closed: bool,
}

impl<W: Write> Encoder<W> {
    pub(crate) fn new_block(writer: W) -> Self {
        Self::new(writer)
    }

    fn new(writer: W) -> Self {
        Self {
            writer,
            header: Header::default(),
            trailer: Trailer::default(),
            hash: Crc64::new(),
            index: BTreeMap::new(),
            compressor: crate::lz4_block::Compressor::default(),
            bytes_written: 0,
            previous_page_number: 0,
            header_written: false,
            closed: false,
        }
    }

    pub(crate) fn encode_header(&mut self, header: Header) -> Result<()> {
        if self.header_written || self.closed {
            return Err(CrabError::LTXCorrupted);
        }
        header.validate()?;
        self.header = header;
        let bytes = header.marshal();
        self.write_hashed(&bytes)?;
        self.header_written = true;
        Ok(())
    }

    pub(crate) fn encode_page(&mut self, mut page: PageHeader, data: &[u8]) -> Result<()> {
        if !self.header_written
            || self.closed
            || page.pgno > self.header.commit
            || data.len() != self.header.page_size as usize
        {
            return Err(CrabError::LTXCorrupted);
        }
        page.validate()?;
        let lock_page = lock_pgno(self.header.page_size);
        if page.pgno == lock_page {
            return Err(CrabError::LTXCorrupted);
        }

        if self.header.is_snapshot() {
            if self.previous_page_number == 0 && page.pgno != 1 {
                return Err(CrabError::LTXCorrupted);
            }
            let expected = if self.previous_page_number == lock_page - 1 {
                self.previous_page_number + 2
            } else {
                self.previous_page_number + 1
            };
            if self.previous_page_number != 0 && page.pgno != expected {
                return Err(CrabError::LTXCorrupted);
            }
        } else if self.previous_page_number >= page.pgno {
            return Err(CrabError::LTXCorrupted);
        }

        let offset = self.bytes_written;
        let compressed = self.compressor.compress(data)?;
        page.flags |= PAGE_HEADER_FLAG_SIZE;
        self.write_hashed(&page.marshal())?;
        let size = u32::try_from(compressed.len())
            .map_err(|error| CrabError::Other(Box::new(error)))?
            .to_be_bytes();
        self.write_hashed(&size)?;
        self.writer.write_all(&compressed)?;
        self.bytes_written += compressed.len() as u64;
        self.hash.update(data);

        self.previous_page_number = page.pgno;
        self.index
            .insert(page.pgno, (offset, self.bytes_written - offset));
        Ok(())
    }

    pub(crate) fn close(&mut self, post_apply_checksum: u64) -> Result<()> {
        if !self.header_written || self.closed {
            return Err(CrabError::LTXCorrupted);
        }

        self.write_hashed(&[0; PAGE_HEADER_SIZE])?;
        let index_offset = self.bytes_written;
        let mut index_bytes = Vec::new();
        for (&page_number, &(offset, size)) in &self.index {
            write_uvarint(&mut index_bytes, page_number as u64);
            write_uvarint(&mut index_bytes, offset);
            write_uvarint(&mut index_bytes, size);
        }
        write_uvarint(&mut index_bytes, 0);
        self.write_hashed(&index_bytes)?;
        self.write_hashed(&(self.bytes_written - index_offset).to_be_bytes())?;

        self.trailer.post_apply_checksum = post_apply_checksum;
        self.hash.update(&post_apply_checksum.to_be_bytes());
        self.trailer.file_checksum = CHECKSUM_FLAG | self.hash.sum64();
        self.trailer.validate(self.header)?;
        if self.header.commit == 0 && post_apply_checksum != CHECKSUM_FLAG {
            return Err(CrabError::LTXCorrupted);
        }
        self.writer.write_all(&self.trailer.marshal())?;
        self.bytes_written += TRAILER_SIZE as u64;
        self.closed = true;
        Ok(())
    }

    fn write_hashed(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.hash.update(bytes);
        self.bytes_written += bytes.len() as u64;
        Ok(())
    }
}

pub(crate) fn decode_page_index(bytes: &[u8]) -> Result<Vec<(u32, u64, u64)>> {
    let mut position = 0;
    let mut entries = Vec::new();
    loop {
        let page_number = read_uvarint(bytes, &mut position)?;
        if page_number == 0 {
            break;
        }
        let offset = read_uvarint(bytes, &mut position)?;
        let size = read_uvarint(bytes, &mut position)?;
        entries.push((
            u32::try_from(page_number).map_err(|_| CrabError::LTXCorrupted)?,
            offset,
            size,
        ));
    }
    if position != bytes.len() {
        return Err(CrabError::LTXCorrupted);
    }
    Ok(entries)
}

fn read_uvarint(bytes: &[u8], position: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*position).ok_or(CrabError::LTXCorrupted)?;
        *position += 1;
        if byte < 0x80 {
            if shift >= 64 || (shift == 63 && byte > 1) {
                return Err(CrabError::LTXCorrupted);
            }
            return Ok(value | (u64::from(byte) << shift));
        }
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if shift >= 70 {
            return Err(CrabError::LTXCorrupted);
        }
    }
}

fn write_uvarint(bytes: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        bytes.push(value as u8 | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
}

struct CountingReader<R> {
    inner: R,
    bytes: u64,
}
impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(bytes)?;
        self.bytes += n as u64;
        Ok(n)
    }
}
