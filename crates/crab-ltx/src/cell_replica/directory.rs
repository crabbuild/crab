use std::collections::BTreeMap;

use crab_storage::{CellObjectKind, CellStorageLayout};

use crate::{CrabError, Host, Result};

const MAGIC: &[u8; 8] = b"CRBDIR01";
const HEADER_BYTES: usize = 32;
const LEAF_RECORD_BYTES: usize = 88;
const BRANCH_RECORD_BYTES: usize = 56;
const FANOUT: usize = 256;
const MAX_NODE_BYTES: u64 = (HEADER_BYTES + FANOUT * LEAF_RECORD_BYTES) as u64;

#[derive(Clone)]
pub(super) struct DirectoryEntry {
    pub page: u32,
    pub object: [u8; 32],
    pub offset: u64,
    pub length: u32,
    pub frame_hash: [u8; 32],
    pub checksum: u64,
}

pub(super) struct DirectoryObject {
    pub digest: [u8; 32],
    pub bytes: Vec<u8>,
}

pub(super) struct DirectoryTree {
    objects: Vec<DirectoryObject>,
    root: Node,
    height: u32,
}

impl DirectoryTree {
    pub(super) fn build(
        entries: BTreeMap<u32, DirectoryEntry>,
        page_size: u32,
        database_pages: u32,
    ) -> Result<Self> {
        if entries.is_empty() || database_pages == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        let lock = crate::ltx::lock_pgno(page_size);
        let expected = u64::from(database_pages) - u64::from(lock <= database_pages);
        if entries.len() as u64 != expected
            || entries
                .keys()
                .any(|page| *page == 0 || *page > database_pages || *page == lock)
        {
            return Err(CrabError::LTXCorrupted);
        }

        let mut objects = Vec::new();
        let mut groups: BTreeMap<u32, Vec<DirectoryEntry>> = BTreeMap::new();
        for entry in entries.into_values() {
            groups
                .entry((entry.page - 1) / FANOUT as u32)
                .or_default()
                .push(entry);
        }
        let mut nodes = groups
            .into_iter()
            .map(|(index, entries)| {
                let bytes = encode_leaf(&entries)?;
                let node = Node::from_bytes(index, &bytes)?;
                objects.push(DirectoryObject {
                    digest: node.digest,
                    bytes,
                });
                Ok(node)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut height = 0;
        while nodes.len() > 1 {
            let mut parents = Vec::new();
            for group in nodes
                .chunk_by(|left, right| left.index / FANOUT as u32 == right.index / FANOUT as u32)
            {
                let index = group[0].index / FANOUT as u32;
                let bytes = encode_branch(group)?;
                let node = Node::from_bytes(index, &bytes)?;
                objects.push(DirectoryObject {
                    digest: node.digest,
                    bytes,
                });
                parents.push(node);
            }
            nodes = parents;
            height += 1;
        }
        let root = nodes.pop().ok_or(CrabError::LTXCorrupted)?;
        Ok(Self {
            objects,
            root,
            height,
        })
    }

    pub(super) fn objects(&self) -> &[DirectoryObject] {
        &self.objects
    }

    pub(super) fn root_digest(&self) -> [u8; 32] {
        self.root.digest
    }

    pub(super) fn height(&self) -> u32 {
        self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Aggregate {
    pub live_pages: u64,
    pub checksum: u64,
    first: u32,
    last: u32,
}

#[derive(Clone)]
struct Node {
    index: u32,
    digest: [u8; 32],
    aggregate: Aggregate,
}

impl Node {
    fn from_bytes(index: u32, bytes: &[u8]) -> Result<Self> {
        let header = Header::parse(bytes)?;
        let (first, last) = if header.kind == 0 {
            let first = read_u32(bytes, HEADER_BYTES)?;
            let last = read_u32(
                bytes,
                HEADER_BYTES + (header.entries as usize - 1) * LEAF_RECORD_BYTES,
            )?;
            (first, last)
        } else {
            let first = read_u32(bytes, HEADER_BYTES)?;
            let last = read_u32(
                bytes,
                HEADER_BYTES
                    + (header.entries as usize - 1) * BRANCH_RECORD_BYTES
                    + size_of::<u32>(),
            )?;
            (first, last)
        };
        Ok(Self {
            index,
            digest: *blake3::hash(bytes).as_bytes(),
            aggregate: Aggregate {
                live_pages: header.live_pages,
                checksum: header.checksum,
                first,
                last,
            },
        })
    }
}

struct Header {
    kind: u8,
    entries: u32,
    live_pages: u64,
    checksum: u64,
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let prefix = bytes.get(..HEADER_BYTES).ok_or(CrabError::LTXCorrupted)?;
        if &prefix[..8] != MAGIC
            || u16::from_be_bytes(array(&prefix[8..10])?) != 1
            || prefix[10] > 1
            || prefix[11] != 0
        {
            return Err(CrabError::LTXCorrupted);
        }
        let entries = u32::from_be_bytes(array(&prefix[12..16])?);
        if entries == 0 || entries > FANOUT as u32 {
            return Err(CrabError::LTXCorrupted);
        }
        let record_bytes = if prefix[10] == 0 {
            LEAF_RECORD_BYTES
        } else {
            BRANCH_RECORD_BYTES
        };
        if bytes.len() != HEADER_BYTES + entries as usize * record_bytes {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(Self {
            kind: prefix[10],
            entries,
            live_pages: u64::from_be_bytes(array(&prefix[16..24])?),
            checksum: u64::from_be_bytes(array(&prefix[24..32])?),
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Verification<'a> {
    pub layout: &'a CellStorageLayout,
    pub cell: &'a [u8; 32],
    pub incarnation: &'a [u8; 16],
    pub page_size: u32,
    pub database_pages: u32,
    pub extents: &'a BTreeMap<[u8; 32], ObjectExtent>,
    pub host: &'a Host,
}

#[derive(Clone, Copy)]
pub(super) struct ObjectExtent {
    pub kind: CellObjectKind,
    pub offset: u64,
    pub length: u64,
}

pub(super) async fn verify_root(
    verification: Verification<'_>,
    root: [u8; 32],
    height: u32,
) -> Result<Aggregate> {
    if height > 3 || verification.database_pages == 0 {
        return Err(CrabError::LTXCorrupted);
    }
    let bytes = read_node(&verification, root).await?;
    let header = Header::parse(&bytes)?;
    if (height == 0) != (header.kind == 0) {
        return Err(CrabError::LTXCorrupted);
    }
    let root_aggregate = if header.kind == 0 {
        verify_leaf(
            &bytes,
            &header,
            verification.page_size,
            verification.database_pages,
            verification.extents,
        )?
        .0
    } else {
        verify_branch(&bytes, &header)?.0
    };
    let lock = crate::ltx::lock_pgno(verification.page_size);
    let expected_pages =
        u64::from(verification.database_pages) - u64::from(lock <= verification.database_pages);
    if root_aggregate.live_pages != expected_pages
        || root_aggregate.first == 0
        || root_aggregate.last > verification.database_pages
    {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok(root_aggregate)
}

pub(super) async fn lookup(
    verification: Verification<'_>,
    root: [u8; 32],
    mut height: u32,
    page: u32,
) -> Result<DirectoryEntry> {
    if page == 0 || page > verification.database_pages {
        return Err(CrabError::TxNotAvailable);
    }
    let mut digest = root;
    let mut expected = None;
    loop {
        let bytes = read_node(&verification, digest).await?;
        let header = Header::parse(&bytes)?;
        if (height == 0) != (header.kind == 0) {
            return Err(CrabError::LTXCorrupted);
        }
        if header.kind == 0 {
            let (aggregate, entries) = verify_leaf(
                &bytes,
                &header,
                verification.page_size,
                verification.database_pages,
                verification.extents,
            )?;
            if expected.is_some_and(|value| value != aggregate) {
                return Err(CrabError::ChecksumMismatch);
            }
            return entries
                .into_iter()
                .find(|entry| entry.page == page)
                .ok_or(CrabError::LTXCorrupted);
        }
        let (aggregate, children) = verify_branch(&bytes, &header)?;
        if expected.is_some_and(|value| value != aggregate) {
            return Err(CrabError::ChecksumMismatch);
        }
        let child = children
            .into_iter()
            .find(|child| child.aggregate.first <= page && page <= child.aggregate.last)
            .ok_or(CrabError::LTXCorrupted)?;
        digest = child.digest;
        expected = Some(child.aggregate);
        height = height.checked_sub(1).ok_or(CrabError::LTXCorrupted)?;
    }
}

pub(super) async fn load_checksums(
    verification: Verification<'_>,
    root: [u8; 32],
    height: u32,
) -> Result<crate::pages::PageChecksums> {
    if height > 3 || verification.database_pages == 0 {
        return Err(CrabError::LTXCorrupted);
    }
    let mut pending = vec![(root, height, None)];
    let mut checksums = vec![0; verification.database_pages as usize];
    let mut previous_page = 0;
    let mut seen = 0u64;
    while let Some((digest, remaining, expected)) = pending.pop() {
        let bytes = read_node(&verification, digest).await?;
        let header = Header::parse(&bytes)?;
        if (remaining == 0) != (header.kind == 0) {
            return Err(CrabError::LTXCorrupted);
        }
        if header.kind == 0 {
            let (aggregate, entries) = verify_leaf(
                &bytes,
                &header,
                verification.page_size,
                verification.database_pages,
                verification.extents,
            )?;
            if expected.is_some_and(|value| value != aggregate) {
                return Err(CrabError::ChecksumMismatch);
            }
            for entry in entries {
                let slot = checksums
                    .get_mut(entry.page as usize - 1)
                    .ok_or(CrabError::LTXCorrupted)?;
                if entry.page <= previous_page || *slot != 0 {
                    return Err(CrabError::LTXCorrupted);
                }
                *slot = entry.checksum;
                previous_page = entry.page;
                seen += 1;
            }
            continue;
        }
        let (aggregate, children) = verify_branch(&bytes, &header)?;
        if expected.is_some_and(|value| value != aggregate) {
            return Err(CrabError::ChecksumMismatch);
        }
        let next = remaining.checked_sub(1).ok_or(CrabError::LTXCorrupted)?;
        pending.extend(
            children
                .into_iter()
                .rev()
                .map(|child| (child.digest, next, Some(child.aggregate))),
        );
    }
    let lock = crate::ltx::lock_pgno(verification.page_size);
    let expected =
        u64::from(verification.database_pages) - u64::from(lock <= verification.database_pages);
    if seen != expected {
        return Err(CrabError::LTXCorrupted);
    }
    crate::pages::PageChecksums::from_dense(verification.page_size, checksums)
}

async fn read_node(verification: &Verification<'_>, digest: [u8; 32]) -> Result<Vec<u8>> {
    let path = verification.layout.incarnation_object_path(
        verification.cell,
        verification.incarnation,
        &digest,
        CellObjectKind::Directory,
    );
    let _permit = verification.host.io_permit().await?;
    let (bytes, _) = verification
        .layout
        .store()
        .get_with_etag_bounded(&path, MAX_NODE_BYTES)
        .await?;
    if *blake3::hash(&bytes).as_bytes() != digest {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok(bytes.to_vec())
}

fn encode_leaf(entries: &[DirectoryEntry]) -> Result<Vec<u8>> {
    if entries.is_empty() || entries.len() > FANOUT {
        return Err(CrabError::LTXCorrupted);
    }
    let live_pages = entries.len() as u64;
    let checksum = entries.iter().fold(0, |sum, entry| sum ^ entry.checksum) | crate::CHECKSUM_FLAG;
    let mut bytes = header(0, entries.len(), live_pages, checksum);
    let mut previous = 0;
    for entry in entries {
        if entry.page <= previous || entry.length == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        bytes.extend_from_slice(&entry.page.to_be_bytes());
        bytes.extend_from_slice(&entry.object);
        bytes.extend_from_slice(&entry.offset.to_be_bytes());
        bytes.extend_from_slice(&entry.length.to_be_bytes());
        bytes.extend_from_slice(&entry.frame_hash);
        bytes.extend_from_slice(&entry.checksum.to_be_bytes());
        previous = entry.page;
    }
    Ok(bytes)
}

fn encode_branch(children: &[Node]) -> Result<Vec<u8>> {
    if children.is_empty() || children.len() > FANOUT {
        return Err(CrabError::LTXCorrupted);
    }
    let live_pages = children.iter().try_fold(0u64, |total, child| {
        total
            .checked_add(child.aggregate.live_pages)
            .ok_or(CrabError::LTXCorrupted)
    })?;
    let checksum = children
        .iter()
        .fold(0, |sum, child| sum ^ child.aggregate.checksum)
        | crate::CHECKSUM_FLAG;
    let mut bytes = header(1, children.len(), live_pages, checksum);
    let mut previous = 0;
    for child in children {
        if child.aggregate.first == 0
            || child.aggregate.first <= previous
            || child.aggregate.last < child.aggregate.first
        {
            return Err(CrabError::LTXCorrupted);
        }
        bytes.extend_from_slice(&child.aggregate.first.to_be_bytes());
        bytes.extend_from_slice(&child.aggregate.last.to_be_bytes());
        bytes.extend_from_slice(&child.digest);
        bytes.extend_from_slice(&child.aggregate.live_pages.to_be_bytes());
        bytes.extend_from_slice(&child.aggregate.checksum.to_be_bytes());
        previous = child.aggregate.last;
    }
    Ok(bytes)
}

fn verify_leaf(
    bytes: &[u8],
    header: &Header,
    page_size: u32,
    database_pages: u32,
    extents: &BTreeMap<[u8; 32], ObjectExtent>,
) -> Result<(Aggregate, Vec<DirectoryEntry>)> {
    let lock = crate::ltx::lock_pgno(page_size);
    let mut checksum = 0;
    let mut first = 0;
    let mut last = 0;
    let mut previous = 0;
    let mut entries = Vec::with_capacity(header.entries as usize);
    for index in 0..header.entries as usize {
        let start = HEADER_BYTES + index * LEAF_RECORD_BYTES;
        let page = read_u32(bytes, start)?;
        let object = array(&bytes[start + 4..start + 36])?;
        let offset = read_u64(bytes, start + 36)?;
        let length = read_u32(bytes, start + 44)?;
        let frame_hash = array(&bytes[start + 48..start + 80])?;
        let page_checksum = read_u64(bytes, start + 80)?;
        let extent = extents.get(&object).ok_or(CrabError::LTXCorrupted)?;
        let object_end = extent
            .offset
            .checked_add(extent.length)
            .ok_or(CrabError::LTXCorrupted)?;
        let frame_end = offset
            .checked_add(u64::from(length))
            .ok_or(CrabError::LTXCorrupted)?;
        if page == 0
            || page > database_pages
            || page == lock
            || page <= previous
            || page_checksum & crate::CHECKSUM_FLAG == 0
            || length == 0
            || offset < extent.offset
            || frame_end > object_end
        {
            return Err(CrabError::LTXCorrupted);
        }
        if first == 0 {
            first = page;
        }
        last = page;
        previous = page;
        checksum ^= page_checksum;
        entries.push(DirectoryEntry {
            page,
            object,
            offset,
            length,
            frame_hash,
            checksum: page_checksum,
        });
    }
    if (first - 1) / FANOUT as u32 != (last - 1) / FANOUT as u32 {
        return Err(CrabError::LTXCorrupted);
    }
    let aggregate = Aggregate {
        live_pages: u64::from(header.entries),
        checksum: checksum | crate::CHECKSUM_FLAG,
        first,
        last,
    };
    if aggregate.live_pages != header.live_pages || aggregate.checksum != header.checksum {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok((aggregate, entries))
}

fn verify_branch(bytes: &[u8], header: &Header) -> Result<(Aggregate, Vec<Node>)> {
    let mut children = Vec::with_capacity(header.entries as usize);
    let mut live_pages = 0u64;
    let mut checksum = 0u64;
    let mut first = 0;
    let mut last = 0;
    for index in 0..header.entries as usize {
        let start = HEADER_BYTES + index * BRANCH_RECORD_BYTES;
        let child_first = read_u32(bytes, start)?;
        let child_last = read_u32(bytes, start + 4)?;
        let digest = array(&bytes[start + 8..start + 40])?;
        let child_pages = read_u64(bytes, start + 40)?;
        let child_checksum = read_u64(bytes, start + 48)?;
        if child_first == 0
            || child_last < child_first
            || child_first <= last
            || child_pages == 0
            || child_checksum & crate::CHECKSUM_FLAG == 0
        {
            return Err(CrabError::LTXCorrupted);
        }
        if first == 0 {
            first = child_first;
        }
        last = child_last;
        live_pages = live_pages
            .checked_add(child_pages)
            .ok_or(CrabError::LTXCorrupted)?;
        checksum ^= child_checksum;
        children.push(Node {
            index: 0,
            digest,
            aggregate: Aggregate {
                live_pages: child_pages,
                checksum: child_checksum,
                first: child_first,
                last: child_last,
            },
        });
    }
    let aggregate = Aggregate {
        live_pages,
        checksum: checksum | crate::CHECKSUM_FLAG,
        first,
        last,
    };
    if aggregate.live_pages != header.live_pages || aggregate.checksum != header.checksum {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok((aggregate, children))
}

fn header(kind: u8, entries: usize, live_pages: u64, checksum: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.push(kind);
    bytes.push(0);
    bytes.extend_from_slice(&(entries as u32).to_be_bytes());
    bytes.extend_from_slice(&live_pages.to_be_bytes());
    bytes.extend_from_slice(&checksum.to_be_bytes());
    bytes
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(array(
        bytes
            .get(offset..offset + size_of::<u32>())
            .ok_or(CrabError::LTXCorrupted)?,
    )?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(array(
        bytes
            .get(offset..offset + size_of::<u64>())
            .ok_or(CrabError::LTXCorrupted)?,
    )?))
}

fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| CrabError::LTXCorrupted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix_tree_round_trips_multi_level_aggregates() {
        let entries = (1..=70_000)
            .map(|page| {
                (
                    page,
                    DirectoryEntry {
                        page,
                        object: [1; 32],
                        offset: u64::from(page) * 100,
                        length: 100,
                        frame_hash: [2; 32],
                        checksum: u64::from(page) | crate::CHECKSUM_FLAG,
                    },
                )
            })
            .collect();
        let tree = DirectoryTree::build(entries, 4096, 70_000).unwrap();
        assert_eq!(tree.height(), 2);
        assert_eq!(tree.root.aggregate.live_pages, 70_000);
        assert!(tree.objects.len() > 256);
    }

    #[test]
    fn radix_tree_rejects_missing_allocated_page() {
        let entries = [1, 3]
            .into_iter()
            .map(|page| {
                (
                    page,
                    DirectoryEntry {
                        page,
                        object: [1; 32],
                        offset: u64::from(page) * 100,
                        length: 100,
                        frame_hash: [2; 32],
                        checksum: u64::from(page) | crate::CHECKSUM_FLAG,
                    },
                )
            })
            .collect();
        assert!(DirectoryTree::build(entries, 4096, 3).is_err());
    }
}
