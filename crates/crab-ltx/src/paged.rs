//! Celld-inspired pinned page maps; authenticated indexes replace unchecked tails.

use crate::{CrabError, Result};

pub(crate) const ENTRY_BYTES: usize = 60;
const FRAME_PREFIX: usize = crate::ltx::PAGE_HEADER_SIZE + 4;

pub(crate) struct IndexEntry {
    pub page: u32,
    pub offset: u64,
    pub size: u64,
    pub hash: [u8; 32],
    pub checksum: u64,
}

struct IndexEntries<'a> {
    chunks: std::slice::Iter<'a, [u8; ENTRY_BYTES]>,
}

impl Iterator for IndexEntries<'_> {
    type Item = Result<IndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.chunks.next().map(|entry| decode_index_entry(entry))
    }
}

pub(crate) fn decode_index_entry(entry: &[u8]) -> Result<IndexEntry> {
    Ok(IndexEntry {
        page: u32::from_be_bytes(array(entry.get(..4).ok_or(CrabError::LTXCorrupted)?)?),
        offset: u64::from_be_bytes(array(entry.get(4..12).ok_or(CrabError::LTXCorrupted)?)?),
        size: u64::from_be_bytes(array(entry.get(12..20).ok_or(CrabError::LTXCorrupted)?)?),
        hash: array(entry.get(20..52).ok_or(CrabError::LTXCorrupted)?)?,
        checksum: u64::from_be_bytes(array(entry.get(52..60).ok_or(CrabError::LTXCorrupted)?)?),
    })
}

pub(crate) struct ValidatedIndexEntries<'a> {
    entries: IndexEntries<'a>,
    validator: IndexValidator,
}

pub(crate) struct IndexValidator {
    info: crate::SegmentInfo,
    previous_page: u32,
    previous_end: u64,
}

impl IndexValidator {
    pub(crate) fn new(info: &crate::SegmentInfo) -> Self {
        Self {
            info: info.clone(),
            previous_page: 0,
            previous_end: crate::ltx::HEADER_SIZE as u64,
        }
    }

    pub(crate) fn validate(&mut self, entry: IndexEntry) -> Result<IndexEntry> {
        let lock = crate::ltx::lock_pgno(self.info.page_size);
        let max_frame = crate::lz4_block::compress_bound(self.info.page_size as usize) as u64
            + FRAME_PREFIX as u64;
        let end = entry
            .offset
            .checked_add(entry.size)
            .ok_or(CrabError::LTXCorrupted)?;
        let footer = crate::ltx::PAGE_HEADER_SIZE + 8 + crate::ltx::TRAILER_SIZE + 1;
        if entry.page <= self.previous_page
            || entry.page > self.info.database_pages
            || entry.page == lock
            || entry.offset != self.previous_end
            || !(FRAME_PREFIX as u64..=max_frame).contains(&entry.size)
            || end > self.info.size_bytes.saturating_sub(footer as u64)
        {
            return Err(CrabError::LTXCorrupted);
        }
        self.previous_page = entry.page;
        self.previous_end = end;
        Ok(entry)
    }
}

impl Iterator for ValidatedIndexEntries<'_> {
    type Item = Result<IndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries
            .next()
            .map(|entry| self.validator.validate(entry?))
    }
}

pub(crate) fn decode_index(bytes: &[u8]) -> Result<Vec<IndexEntry>> {
    index_entries(bytes)?.collect()
}

fn index_entries(bytes: &[u8]) -> Result<IndexEntries<'_>> {
    let (entries, remainder) = bytes.as_chunks::<ENTRY_BYTES>();
    if !remainder.is_empty() {
        return Err(CrabError::LTXCorrupted);
    }
    Ok(IndexEntries {
        chunks: entries.iter(),
    })
}

pub(crate) fn validated_index_entries<'a>(
    bytes: &'a [u8],
    info: &'a crate::SegmentInfo,
) -> Result<ValidatedIndexEntries<'a>> {
    Ok(ValidatedIndexEntries {
        entries: index_entries(bytes)?,
        validator: IndexValidator::new(info),
    })
}

/// Encodes the fixed-size authenticated index collected by the streaming LTX
/// decoder. The decoder has already verified the embedded variable-length
/// index and trailer; this pass only changes its representation for paged SQL.
pub(crate) fn encode_index_from_pages(pages: &[crate::codec::EncodedPage]) -> Result<Vec<u8>> {
    if pages.is_empty() {
        return Err(CrabError::LTXCorrupted);
    }
    let mut index = Vec::with_capacity(
        pages
            .len()
            .checked_mul(ENTRY_BYTES)
            .ok_or(CrabError::Limit("LTX page index bytes"))?,
    );
    for page in pages {
        if page.frame_hash == [0; 32] || page.size == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        index.extend_from_slice(&page.page.to_be_bytes());
        index.extend_from_slice(&page.offset.to_be_bytes());
        index.extend_from_slice(&page.size.to_be_bytes());
        index.extend_from_slice(&page.frame_hash);
        index.extend_from_slice(&page.checksum.to_be_bytes());
    }
    Ok(index)
}

fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| CrabError::LTXCorrupted)
}

pub(crate) fn decode_frame(frame: &[u8], page_size: u32, pgno: u32) -> Result<Vec<u8>> {
    let prefix = frame.get(..FRAME_PREFIX).ok_or(CrabError::LTXCorrupted)?;
    let header = crate::ltx::PageHeader::parse(&prefix[..crate::ltx::PAGE_HEADER_SIZE])?;
    header.validate()?;
    let size =
        u32::from_be_bytes(array(&prefix[crate::ltx::PAGE_HEADER_SIZE..FRAME_PREFIX])?) as usize;
    if header.pgno != pgno
        || header.flags != crate::ltx::PAGE_HEADER_FLAG_SIZE
        || size != frame.len() - FRAME_PREFIX
        || size > crate::lz4_block::compress_bound(page_size as usize)
    {
        return Err(CrabError::LTXCorrupted);
    }
    let mut data = vec![0; page_size as usize];
    let n = lz4_flex::block::decompress_into(&frame[FRAME_PREFIX..], &mut data)
        .map_err(|e| CrabError::Other(Box::new(e)))?;
    if n != data.len() {
        return Err(CrabError::LTXCorrupted);
    }
    Ok(data)
}
