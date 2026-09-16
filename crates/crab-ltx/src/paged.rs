//! Celld-inspired pinned page maps; authenticated indexes replace unchecked tails.

use crate::{CrabError, Position, Replica, Result, replica::RemoteSegment};
use std::sync::Arc;

mod map;
use map::PageMap;

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

#[derive(Clone)]
struct Locator {
    segment: Arc<RemoteSegment>,
    offset: u64,
    size: u64,
    hash: [u8; 32],
    checksum: u64,
}

/// Immutable exact-cut page reads backed by authenticated object-store ranges.
///
/// The head and index are trusted publication metadata, not signatures. Access
/// control and retention of every referenced object remain the caller's job.
/// No full LTX body is downloaded during construction; each page read verifies
/// the frame's BLAKE3 and the decoded page checksum before returning bytes.
#[derive(Clone)]
pub struct PagedDatabase {
    replica: Replica,
    pages: Arc<PageMap>,
    page_size: u32,
    count: u32,
    position: Position,
}

impl PagedDatabase {
    pub(crate) fn belongs_to(&self, replica: &Replica) -> bool {
        Arc::ptr_eq(&self.replica.identity, &replica.identity)
    }
    pub(crate) fn host(&self) -> crate::Host {
        self.replica.host.clone()
    }
    pub(crate) fn checksums(&self) -> Result<crate::pages::PageChecksums> {
        crate::pages::PageChecksums::from_checksums(
            self.page_size,
            self.count,
            self.pages.iter().map(|(p, loc)| (*p, loc.checksum)),
        )
    }

    /// Activates a new writable sparse database continuing this exact cut.
    ///
    /// The directory is exclusive to the caller. Use only the returned managed
    /// writer: opening the sparse file through SQLite's default VFS reads holes.
    pub fn open_writable(self, destination: &std::path::Path) -> Result<crate::ManagedDb> {
        crate::ManagedDb::open_paged(self, destination)
    }

    pub(crate) fn limits(&self) -> crate::Limits {
        self.replica.limits
    }
    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }
    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }
    #[must_use]
    pub fn page_count(&self) -> u32 {
        self.count
    }

    /// Reads a one-based page; pages outside the pinned database are rejected.
    pub async fn read_page(&self, page: u32) -> Result<Vec<u8>> {
        if page == 0 || page > self.count {
            return Err(CrabError::TxNotAvailable);
        }
        if page == crate::ltx::lock_pgno(self.page_size) {
            return Ok(vec![0; self.page_size as usize]);
        }
        let loc = self.pages.get(&page).ok_or(CrabError::LTXCorrupted)?;
        let frame = self
            .replica
            .frame(&loc.segment, loc.offset, loc.size)
            .await?;
        if *blake3::hash(&frame).as_bytes() != loc.hash {
            return Err(CrabError::ChecksumMismatch);
        }
        let bytes = decode_frame(&frame, self.page_size, page)?;
        if crate::ltx::checksum_page(page, &bytes) != loc.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(bytes)
    }

    /// Reads adjacent pages whose compressed frames occupy one object range.
    ///
    /// The run is capped at one MiB decoded, and every returned frame is checked.
    /// Compaction, bundles and inherited epochs share this same locator path.
    pub async fn read_run(&self, first: u32, max_pages: u32) -> Result<Vec<(u32, Vec<u8>)>> {
        if max_pages == 0 {
            return Ok(Vec::new());
        }
        if first == crate::ltx::lock_pgno(self.page_size) {
            return Ok(vec![(first, self.read_page(first).await?)]);
        }
        let first_loc = self.pages.get(&first).ok_or(CrabError::TxNotAvailable)?;
        let count = max_pages
            .min((1 << 20) / self.page_size)
            .min(self.count - first + 1);
        let mut end = first_loc.offset + first_loc.size;
        let mut last = first;
        for page in (first..=first + (count - 1)).skip(1) {
            let Some(loc) = self.pages.get(&page) else {
                break;
            };
            if !Arc::ptr_eq(&loc.segment, &first_loc.segment) || loc.offset != end {
                break;
            }
            end = end.checked_add(loc.size).ok_or(CrabError::LTXCorrupted)?;
            last = page;
        }
        let frames = self
            .replica
            .frame(&first_loc.segment, first_loc.offset, end - first_loc.offset)
            .await?;
        let mut output = Vec::new();
        for page in first..=last {
            let loc = self.pages.get(&page).ok_or(CrabError::LTXCorrupted)?;
            let start = (loc.offset - first_loc.offset) as usize;
            let frame = frames
                .get(start..start + loc.size as usize)
                .ok_or(CrabError::LTXCorrupted)?;
            if *blake3::hash(frame).as_bytes() != loc.hash {
                return Err(CrabError::ChecksumMismatch);
            }
            let bytes = decode_frame(frame, self.page_size, page)?;
            if crate::ltx::checksum_page(page, &bytes) != loc.checksum {
                return Err(CrabError::ChecksumMismatch);
            }
            output.push((page, bytes));
        }
        Ok(output)
    }

    /// Opens a read-only SQLite VFS over this pinned cut, with no local database.
    ///
    /// SQLite calls are blocking. Run queries on a dedicated or blocking thread.
    /// The VFS shares an independent I/O worker, never a caller runtime's block_on.
    pub fn open_sqlite(self) -> Result<crate::PagedConnection> {
        crate::paged_vfs::open(self)
    }
}

pub(crate) async fn build(
    replica: Replica,
    segments: &[RemoteSegment],
    position: Position,
) -> Result<PagedDatabase> {
    let indexes = crate::replica::io::ordered(segments.iter().cloned().map(|segment| {
        let replica = replica.clone();
        async move {
            let bytes = replica.index(&segment).await?;
            Ok((segment, bytes))
        }
    }))
    .await?;
    replica
        .host
        .clone()
        .run(move || extend(replica, None, indexes, position))
        .await?
}

pub(crate) fn extend(
    replica: Replica,
    base: Option<PagedDatabase>,
    indexes: Vec<(RemoteSegment, Vec<u8>)>,
    position: Position,
) -> Result<PagedDatabase> {
    let (mut pages, mut count, mut page_size, mut previous_position) = match base {
        Some(base) => (
            Arc::unwrap_or_clone(base.pages),
            base.count,
            base.page_size,
            base.position,
        ),
        None => (PageMap::default(), 0, 0, Position::default()),
    };
    for (segment, bytes) in indexes {
        let segment = Arc::new(segment);
        let info = &segment.info;
        if previous_position.txid.checked_add(1) != Some(info.min_txid)
            || info.max_txid < info.min_txid
            || info.pre_checksum != previous_position.checksum
            || (page_size != 0 && page_size != info.page_size)
        {
            return Err(CrabError::LTXCorrupted);
        }
        page_size = info.page_size;
        count = info.database_pages;
        // Apply each truncation before the next delta; a later regrowth must not
        // revive old pages from a version predating that truncation.
        pages.truncate(count);
        for entry in validated_index_entries(&bytes, info)? {
            let entry = entry?;
            let pgno = entry.page;
            let offset = entry.offset;
            let size = entry.size;
            pages.insert(
                pgno,
                Locator {
                    segment: segment.clone(),
                    offset,
                    size,
                    hash: entry.hash,
                    checksum: entry.checksum,
                },
            );
        }
        let lock = crate::ltx::lock_pgno(page_size);
        if pages.len() as u64 != u64::from(count) - u64::from(lock <= count) {
            return Err(CrabError::LTXCorrupted);
        }
        if pages.checksum() != info.post_checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        previous_position = info.position();
    }
    if previous_position != position {
        return Err(CrabError::ChecksumMismatch);
    }
    // A returned view may live for days; it must not retain a temporary
    // recovery reservation from the operation that constructed its page map.
    let host = replica.host.clone().without_recovery();
    Ok(PagedDatabase {
        replica: replica.with_host(host),
        pages: Arc::new(pages),
        page_size,
        count,
        position,
    })
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

// The sidecar binds offsets and compressed-frame digests to the CAS head. The
// LTX whole-file digest alone cannot authenticate an isolated range read.
pub(crate) fn encode_index(bytes: &[u8]) -> Result<Vec<u8>> {
    let (_, pages) = crate::ltx::decode_file_with_pages(bytes)?;
    let field = bytes
        .len()
        .checked_sub(crate::ltx::TRAILER_SIZE + 8)
        .ok_or(CrabError::LTXCorrupted)?;
    let len = usize::try_from(u64::from_be_bytes(array(&bytes[field..field + 8])?))
        .map_err(|_| CrabError::LTXCorrupted)?;
    let start = field.checked_sub(len).ok_or(CrabError::LTXCorrupted)?;
    let entries = crate::codec::decode_page_index(&bytes[start..field])?;
    if entries.len() != pages.len() {
        return Err(CrabError::LTXCorrupted);
    }
    let mut index = Vec::new();
    for ((pgno, offset, size), (page, data)) in entries.into_iter().zip(pages) {
        if pgno != page {
            return Err(CrabError::LTXCorrupted);
        }
        let end = offset.checked_add(size).ok_or(CrabError::LTXCorrupted)?;
        let frame = bytes
            .get(offset as usize..end as usize)
            .ok_or(CrabError::LTXCorrupted)?;
        // Replica writers use sized-block LTX; the old frame decoder remains
        // supported by exact recovery, not by this new paged storage contract.
        decode_frame(frame, data.len() as u32, pgno)?;
        index.extend_from_slice(&pgno.to_be_bytes());
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(&size.to_be_bytes());
        index.extend_from_slice(blake3::hash(frame).as_bytes());
        index.extend_from_slice(&crate::ltx::checksum_page(pgno, &data).to_be_bytes());
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
