use std::{collections::BTreeMap, sync::Arc};

use crate::{CellObjectKind, CellStorageLayout, CrabError, Host, Result};

mod checksums;
mod initial;
mod relocate;
mod update;

pub(super) use checksums::load_checksums;
pub(super) use initial::{
    build_and_upload as build_initial_and_upload, entries as initial_entries,
};
pub(super) use relocate::run as relocate_and_upload;

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

pub(super) struct DirectorySpan {
    pub object: [u8; 32],
    pub start: u64,
    pub end: u64,
    pub entries: Vec<DirectoryEntry>,
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
    #[cfg(test)]
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

    pub(super) fn checksum(&self) -> u64 {
        self.root.aggregate.checksum
    }

    pub(super) async fn update(
        base: Verification<'_>,
        root: [u8; 32],
        height: u32,
        aggregate: Aggregate,
        changes: BTreeMap<u32, DirectoryEntry>,
        retain_through: u32,
        final_state: Verification<'_>,
        expected_checksum: u64,
    ) -> Result<Self> {
        update::run(
            base,
            root,
            height,
            aggregate,
            changes,
            retain_through,
            final_state,
            expected_checksum,
        )
        .await
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
    pub origin: crate::LtxReadOrigin,
}

#[derive(Clone)]
pub(super) struct ObjectExtent {
    pub kind: CellObjectKind,
    pub ranges: Vec<std::ops::Range<u64>>,
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

pub(super) async fn reachable_digests(
    verification: Verification<'_>,
    root: [u8; 32],
    height: u32,
    expected_root: Aggregate,
) -> Result<Vec<[u8; 32]>> {
    if height > 3 || verification.database_pages == 0 {
        return Err(CrabError::LTXCorrupted);
    }
    let mut pending = vec![(root, height, Some(expected_root))];
    let mut digests = Vec::new();
    let mut previous_page = 0u32;
    let mut seen = 0u64;
    let mut checksum = crate::CHECKSUM_FLAG;
    while let Some((digest, remaining, expected)) = pending.pop() {
        // Backup/collection inventory must prove the origin still contains
        // every dependency; a process cache cannot establish remote presence.
        let bytes = read_node_uncached(&verification, digest).await?;
        let header = Header::parse(&bytes)?;
        if (remaining == 0) != (header.kind == 0) {
            return Err(CrabError::LTXCorrupted);
        }
        digests.push(digest);
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
                let expected_page = previous_page
                    .checked_add(1)
                    .ok_or(CrabError::LTXCorrupted)?;
                let lock = crate::ltx::lock_pgno(verification.page_size);
                if expected_page == lock {
                    previous_page = lock;
                }
                if entry.page
                    != previous_page
                        .checked_add(1)
                        .ok_or(CrabError::LTXCorrupted)?
                {
                    return Err(CrabError::LTXCorrupted);
                }
                checksum = crate::CHECKSUM_FLAG | (checksum ^ entry.checksum);
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
    if previous_page < verification.database_pages {
        if previous_page
            .checked_add(1)
            .ok_or(CrabError::LTXCorrupted)?
            != lock
            || lock != verification.database_pages
        {
            return Err(CrabError::LTXCorrupted);
        }
        previous_page = lock;
    }
    let expected_pages =
        u64::from(verification.database_pages) - u64::from(lock <= verification.database_pages);
    if previous_page != verification.database_pages
        || seen != expected_pages
        || seen != expected_root.live_pages
        || checksum != expected_root.checksum
    {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok(digests)
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

/// Returns one contiguous, authenticated directory run without re-walking the
/// radix path for every page in the window.
pub(super) async fn lookup_run(
    verification: Verification<'_>,
    root: [u8; 32],
    height: u32,
    first: u32,
    max_pages: u32,
) -> Result<Vec<DirectoryEntry>> {
    if max_pages == 0 || first == 0 || first > verification.database_pages {
        return Ok(Vec::new());
    }
    let lock = crate::ltx::lock_pgno(verification.page_size);
    let requested_last = first
        .checked_add(max_pages - 1)
        .ok_or(CrabError::LTXCorrupted)?
        .min(verification.database_pages);
    let last = if (first..=requested_last).contains(&lock) {
        lock.checked_sub(1).ok_or(CrabError::LTXCorrupted)?
    } else {
        requested_last
    };
    if last < first {
        return Ok(Vec::new());
    }

    let mut pending = vec![(root, height, None)];
    let mut entries = Vec::new();
    while let Some((digest, remaining, expected)) = pending.pop() {
        let bytes = read_node(&verification, digest).await?;
        let header = Header::parse(&bytes)?;
        if (remaining == 0) != (header.kind == 0) {
            return Err(CrabError::LTXCorrupted);
        }
        if header.kind == 0 {
            let (aggregate, leaf_entries) = verify_leaf(
                &bytes,
                &header,
                verification.page_size,
                verification.database_pages,
                verification.extents,
            )?;
            if expected.is_some_and(|value| value != aggregate) {
                return Err(CrabError::ChecksumMismatch);
            }
            entries.extend(
                leaf_entries
                    .into_iter()
                    .filter(|entry| (first..=last).contains(&entry.page)),
            );
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
                .filter(|child| child.aggregate.last >= first && child.aggregate.first <= last)
                .map(|child| (child.digest, next, Some(child.aggregate))),
        );
    }

    let expected_entries =
        usize::try_from(last - first + 1).map_err(|_| CrabError::LTXCorrupted)?;
    if entries.len() != expected_entries {
        return Err(CrabError::LTXCorrupted);
    }
    for (index, entry) in entries.iter().enumerate() {
        let expected_page = first
            .checked_add(u32::try_from(index).map_err(|_| CrabError::LTXCorrupted)?)
            .ok_or(CrabError::LTXCorrupted)?;
        if entry.page != expected_page {
            return Err(CrabError::LTXCorrupted);
        }
    }
    Ok(entries)
}

/// Returns every authenticated same-object span in one fixed page window.
pub(super) async fn lookup_spans(
    verification: Verification<'_>,
    root: [u8; 32],
    height: u32,
    first: u32,
    max_pages: u32,
) -> Result<Vec<DirectorySpan>> {
    let entries = lookup_run(verification, root, height, first, max_pages).await?;
    let mut spans: Vec<DirectorySpan> = Vec::new();
    for entry in entries {
        let end = entry
            .offset
            .checked_add(u64::from(entry.length))
            .ok_or(CrabError::LTXCorrupted)?;
        if let Some(span) = spans.last_mut()
            && span.object == entry.object
            && span.end == entry.offset
        {
            span.end = end;
            span.entries.push(entry);
            continue;
        }
        spans.push(DirectorySpan {
            object: entry.object,
            start: entry.offset,
            end,
            entries: vec![entry],
        });
    }
    Ok(spans)
}

async fn read_node(verification: &Verification<'_>, digest: [u8; 32]) -> Result<Arc<[u8]>> {
    let path = verification.layout.incarnation_object_path(
        verification.cell,
        verification.incarnation,
        &digest,
        CellObjectKind::Directory,
    );
    if let Some(bytes) = super::cache::get(
        verification.layout,
        verification.cell,
        verification.incarnation,
        digest,
        CellObjectKind::Directory,
    )? {
        return Ok(bytes);
    }
    let persistent_key = format!(
        "v1:{}:{}",
        path,
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    if let Some(bytes) = verification
        .host
        .directory_cache_get(persistent_key.clone(), MAX_NODE_BYTES)
        .await?
    {
        if bytes.len() <= MAX_NODE_BYTES as usize && *blake3::hash(&bytes).as_bytes() == digest {
            let bytes: Arc<[u8]> = bytes.into();
            return super::cache::insert(
                verification.layout,
                verification.cell,
                verification.incarnation,
                digest,
                CellObjectKind::Directory,
                bytes,
            );
        }
        let _ = verification
            .host
            .directory_cache_invalidate(persistent_key.clone())
            .await;
    }
    let permit = verification.host.io_permit().await?;
    let result = verification
        .layout
        .store()
        .get_with_etag_bounded(&path, MAX_NODE_BYTES)
        .await;
    verification.host.observe_ltx_origin_request(
        verification.origin,
        result.is_ok(),
        result.as_ref().map_or(0, |(bytes, _)| bytes.len()),
    );
    let (bytes, _) = result?;
    if *blake3::hash(&bytes).as_bytes() != digest {
        return Err(CrabError::ChecksumMismatch);
    }
    // Verified bytes no longer need origin admission. Cache fills admit their
    // own bounded job without queuing, so slow disk cannot occupy network slots.
    drop(permit);
    let _ = verification
        .host
        .directory_cache_put(persistent_key, bytes.to_vec(), MAX_NODE_BYTES)
        .await;
    let bytes: Arc<[u8]> = bytes.to_vec().into();
    super::cache::insert(
        verification.layout,
        verification.cell,
        verification.incarnation,
        digest,
        CellObjectKind::Directory,
        bytes,
    )
}

async fn read_node_uncached(
    verification: &Verification<'_>,
    digest: [u8; 32],
) -> Result<Arc<[u8]>> {
    let path = verification.layout.incarnation_object_path(
        verification.cell,
        verification.incarnation,
        &digest,
        CellObjectKind::Directory,
    );
    let _permit = verification.host.io_permit().await?;
    let result = verification
        .layout
        .store()
        .get_with_etag_bounded(&path, MAX_NODE_BYTES)
        .await;
    verification.host.observe_ltx_origin_request(
        verification.origin,
        result.is_ok(),
        result.as_ref().map_or(0, |(bytes, _)| bytes.len()),
    );
    let (bytes, _) = result?;
    if *blake3::hash(&bytes).as_bytes() != digest {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok(bytes.to_vec().into())
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
        let frame_end = offset
            .checked_add(u64::from(length))
            .ok_or(CrabError::LTXCorrupted)?;
        if page == 0
            || page > database_pages
            || page == lock
            || page <= previous
            || page_checksum & crate::CHECKSUM_FLAG == 0
            || length == 0
            || !extent
                .ranges
                .iter()
                .any(|range| range.start <= offset && frame_end <= range.end)
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

/// Shape-checks one encoded directory node without a root graph.
///
/// Authentication needs the exact root's object extents, which this entry point
/// does not have: a leaf is checked against extents derived from its own
/// records, so every structural rule still runs while extent membership and the
/// lock-page rule stay unverified. Audit tooling that must authenticate a node
/// walks the root graph instead.
pub(crate) fn inspect_node(bytes: &[u8]) -> Result<()> {
    let header = Header::parse(bytes)?;
    if header.kind == 1 {
        verify_branch(bytes, &header)?;
        return Ok(());
    }
    let mut extents: BTreeMap<[u8; 32], ObjectExtent> = BTreeMap::new();
    let mut database_pages = 0_u32;
    for index in 0..header.entries as usize {
        let start = HEADER_BYTES + index * LEAF_RECORD_BYTES;
        let page = read_u32(bytes, start)?;
        let object = array(&bytes[start + 4..start + 36])?;
        let offset = read_u64(bytes, start + 36)?;
        let length = read_u32(bytes, start + 44)?;
        let end = offset
            .checked_add(u64::from(length))
            .ok_or(CrabError::LTXCorrupted)?;
        database_pages = database_pages.max(page);
        extents
            .entry(object)
            .or_insert_with(|| ObjectExtent {
                kind: crate::CellObjectKind::Ltx,
                ranges: Vec::new(),
            })
            .ranges
            .push(offset..end);
    }
    // Page size zero disables the lock-page membership rule: with no root graph
    // there is no page size to work from, and every other rule still runs.
    verify_leaf(bytes, &header, 0, database_pages, &extents)?;
    Ok(())
}

#[cfg(test)]
mod tests;
