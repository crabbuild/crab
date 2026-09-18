// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.

use crate::CHECKSUM_FLAG;
use crate::environment::FileIo;
use crate::error::{CrabError, Result};
use crate::ltx::{
    CHECKSUM_SIZE, Crc64, HEADER_SIZE, Header, PAGE_HEADER_FLAG_SIZE, PAGE_HEADER_SIZE, PageHeader,
    TRAILER_SIZE, Trailer, checksum_page, lock_pgno,
};
use std::io::{Read, Write};

const INDEX_COPY_BYTES: usize = 64 << 10;

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
    #[cfg(feature = "replica")]
    replica_index: Vec<EncodedPage>,
}

impl<R: Read> Decoder<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader: CountingReader {
                inner: reader,
                bytes: 0,
                digest: blake3::Hasher::new(),
            },
            index: Vec::new(),
            state: DecoderState::Header,
            header: Header::default(),
            trailer: Trailer::default(),
            hash: Crc64::new(),
            rolling_checksum: 0,
            #[cfg(feature = "replica")]
            replica_index: Vec::new(),
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
            let mut frame_hash = blake3::Hasher::new();
            frame_hash.update(&header_bytes);
            frame_hash.update(&size_bytes);
            frame_hash.update(&compressed);
            let n = lz4_flex::block::decompress_into(&compressed, data)
                .map_err(|error| CrabError::Other(Box::new(error)))?;
            if n != data.len() {
                return Err(CrabError::LTXCorrupted);
            }
            #[cfg(feature = "replica")]
            self.replica_index.push(EncodedPage {
                page: page.pgno,
                offset,
                size: self.reader.bytes - offset,
                frame_hash: *frame_hash.finalize().as_bytes(),
                checksum: checksum_page(page.pgno, data),
            });
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
            #[cfg(feature = "replica")]
            self.replica_index.push(EncodedPage {
                page: page.pgno,
                offset,
                size: self.reader.bytes - offset,
                frame_hash: [0; 32],
                checksum: checksum_page(page.pgno, data),
            });
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

    pub(crate) fn artifact(&self) -> Result<(u64, [u8; 32])> {
        if self.state != DecoderState::Closed {
            return Err(CrabError::InvalidState("LTX decoder is not closed"));
        }
        Ok((
            self.reader.bytes,
            *self.reader.digest.clone().finalize().as_bytes(),
        ))
    }

    #[cfg(feature = "replica")]
    pub(crate) fn replica_index(&self) -> &[EncodedPage] {
        &self.replica_index
    }
}

pub(crate) struct Encoder<W> {
    pub(crate) writer: W,
    pub(crate) header: Header,
    pub(crate) trailer: Trailer,
    hash: Crc64,
    index: EncoderIndex,
    compressor: crate::lz4_block::Compressor,
    bytes_written: u64,
    previous_page_number: u32,
    header_written: bool,
    closed: bool,
}

#[derive(Clone)]
#[cfg_attr(not(feature = "replica"), expect(dead_code))]
pub(crate) struct EncodedPage {
    pub(crate) page: u32,
    pub(crate) offset: u64,
    pub(crate) size: u64,
    pub(crate) frame_hash: [u8; 32],
    pub(crate) checksum: u64,
}

enum EncoderIndex {
    Memory(Vec<u8>),
    File(Box<dyn FileIo>),
}

impl EncoderIndex {
    fn write_entry(&mut self, page: u32, offset: u64, size: u64) -> Result<()> {
        let mut bytes = Vec::with_capacity(30);
        write_uvarint(&mut bytes, u64::from(page));
        write_uvarint(&mut bytes, offset);
        write_uvarint(&mut bytes, size);
        self.write_all(&bytes)
    }

    fn finish(&mut self) -> Result<()> {
        self.write_all(&[0])
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Memory(index) => index.extend_from_slice(bytes),
            Self::File(index) => index.write_all(bytes)?,
        }
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        match self {
            Self::Memory(index) => Ok(index.len() as u64),
            Self::File(index) => Ok(index.file_len()?),
        }
    }

    fn read_exact_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>> {
        match self {
            Self::Memory(index) => {
                let start = usize::try_from(offset).map_err(|_| CrabError::LTXCorrupted)?;
                let end = start.checked_add(length).ok_or(CrabError::LTXCorrupted)?;
                Ok(index
                    .get(start..end)
                    .ok_or(CrabError::LTXCorrupted)?
                    .to_vec())
            }
            Self::File(index) => Ok(index.read_exact_at(offset, length)?),
        }
    }
}

impl<W: Write> Encoder<W> {
    pub(crate) fn new_block(writer: W) -> Self {
        Self::new(writer, EncoderIndex::Memory(Vec::new()))
    }

    pub(crate) fn new_block_spooled(writer: W, index: Box<dyn FileIo>) -> Self {
        Self::new(writer, EncoderIndex::File(index))
    }

    fn new(writer: W, index: EncoderIndex) -> Self {
        Self {
            writer,
            header: Header::default(),
            trailer: Trailer::default(),
            hash: Crc64::new(),
            index,
            compressor: crate::lz4_block::Compressor::default(),
            bytes_written: 0,
            previous_page_number: 0,
            header_written: false,
            closed: false,
        }
    }

    pub(crate) fn into_writer(self) -> W {
        self.writer
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

    pub(crate) fn encode_page(&mut self, mut page: PageHeader, data: &[u8]) -> Result<EncodedPage> {
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
        let header = page.marshal();
        self.write_hashed(&header)?;
        let size = u32::try_from(compressed.len())
            .map_err(|error| CrabError::Other(Box::new(error)))?
            .to_be_bytes();
        self.write_hashed(&size)?;
        self.writer.write_all(&compressed)?;
        self.bytes_written += compressed.len() as u64;
        self.hash.update(data);

        self.previous_page_number = page.pgno;
        let frame_size = self.bytes_written - offset;
        self.index.write_entry(page.pgno, offset, frame_size)?;
        let mut frame_hash = blake3::Hasher::new();
        frame_hash.update(&header);
        frame_hash.update(&size);
        frame_hash.update(&compressed);
        Ok(EncodedPage {
            page: page.pgno,
            offset,
            size: frame_size,
            frame_hash: *frame_hash.finalize().as_bytes(),
            checksum: checksum_page(page.pgno, data),
        })
    }

    pub(crate) fn close(&mut self, post_apply_checksum: u64) -> Result<()> {
        if !self.header_written || self.closed {
            return Err(CrabError::LTXCorrupted);
        }

        self.write_hashed(&[0; PAGE_HEADER_SIZE])?;
        let index_offset = self.bytes_written;
        self.index.finish()?;
        let index_length = self.index.len()?;
        let mut copied = 0_u64;
        while copied < index_length {
            let length = usize::try_from((index_length - copied).min(INDEX_COPY_BYTES as u64))
                .map_err(|_| CrabError::LTXCorrupted)?;
            let bytes = self.index.read_exact_at(copied, length)?;
            self.write_hashed(&bytes)?;
            copied += length as u64;
        }
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
    digest: blake3::Hasher,
}
impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(bytes)?;
        self.bytes += n as u64;
        self.digest.update(&bytes[..n]);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Txid, ltx};

    #[test]
    fn file_spooled_index_matches_in_memory_encoder_bytes() {
        let header = ltx::Header {
            version: ltx::VERSION,
            page_size: 4_096,
            commit: 2,
            min_txid: Txid(1),
            max_txid: Txid(1),
            ..ltx::Header::default()
        };
        let pages = [(1, vec![1; 4_096]), (2, vec![2; 4_096])];
        let checksum = pages.iter().fold(CHECKSUM_FLAG, |checksum, (page, data)| {
            CHECKSUM_FLAG | (checksum ^ ltx::checksum_page(*page, data))
        });
        let encode = |mut encoder: Encoder<Vec<u8>>| {
            encoder.encode_header(header).unwrap();
            for (page, data) in &pages {
                encoder
                    .encode_page(
                        ltx::PageHeader {
                            pgno: *page,
                            flags: 0,
                        },
                        data,
                    )
                    .unwrap();
            }
            encoder.close(checksum).unwrap();
            encoder.into_writer()
        };
        let expected = encode(Encoder::new_block(Vec::new()));
        let directory = tempfile::TempDir::new().unwrap();
        let index_path = directory.path().join("index");
        let index = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(index_path)
            .unwrap();
        let actual = encode(Encoder::new_block_spooled(Vec::new(), Box::new(index)));

        assert_eq!(actual, expected);
        ltx::decode_file(&actual).unwrap();
    }
}
