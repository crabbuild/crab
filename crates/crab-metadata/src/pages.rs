//! Authenticated bounded pages shared by immutable metadata indexes.
use serde::{Deserialize, Serialize};

#[cfg(feature = "storage")]
mod cache;
#[cfg(feature = "storage")]
pub(crate) use cache::{PageCache, PageWork};

use crate::error::{MetadataError, Result};
use crate::validation::corrupt_object;

fn corruption(reason: &str) -> MetadataError {
    crate::validation::corrupt_object("metadata page", reason)
}

fn corrupt_at<T>(path: &str, reason: &str) -> Result<T> {
    Err(corrupt_object(path, reason))
}

const MAGIC: &[u8; 8] = b"CRABPP01";
const HEADER: usize = 14;
pub(crate) const TARGET_BYTES: usize = 32 * 1024;
pub(crate) const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_HEIGHT: u8 = 16;

/// Authenticated location and key coverage of one immutable page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRef {
    pub offset: u64,
    pub bytes: u32,
    pub hash: [u8; 32],
    pub height: u8,
    pub count: u32,
    pub first: Vec<u8>,
    pub last: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    Records = 0,
    Nodes = 1,
    Children = 2,
    Names = 3,
    CommitRecords = 4,
    CommitOids = 5,
    CommitAncestryRoots = 6,
    CommitAncestryNodes = 7,
    #[cfg(feature = "remote-index")]
    ObjectLocators = 8,
    #[cfg(feature = "remote-index")]
    PackOffsets = 9,
    #[cfg(feature = "remote-index")]
    PackInventory = 10,
    #[cfg(feature = "remote-index")]
    PackSlots = 11,
    RunAdmission = 12,
    RunAdmissionCounts = 13,
    VisibilityRefs = 14,
    VisibilityChanges = 15,
    CheckpointObjects = 16,
    CheckpointTransitions = 17,
    CheckpointRefs = 18,
    CheckpointSets = 19,
    CatalogFiles = 20,
    CatalogShards = 21,
    CatalogXorbs = 22,
    CatalogChunks = 23,
    CatalogXorbChunks = 24,
    CatalogShardXorbs = 25,
    RunCatalogFiles = 26,
    RunCatalogShards = 27,
    RunCatalogXorbs = 28,
    RunCatalogChunks = 29,
}

impl Kind {
    fn is_positional(self) -> bool {
        match self {
            Self::Records
            | Self::Nodes
            | Self::CommitRecords
            | Self::CommitAncestryRoots
            | Self::RunAdmissionCounts
            | Self::CheckpointTransitions
            | Self::CatalogXorbChunks => true,
            #[cfg(feature = "remote-index")]
            Self::PackSlots => true,
            _ => false,
        }
    }

    pub(crate) fn key_bytes(self) -> usize {
        match self {
            Self::Names
            | Self::CommitAncestryNodes
            | Self::VisibilityRefs
            | Self::CheckpointRefs
            | Self::CatalogFiles
            | Self::CatalogShards
            | Self::CatalogXorbs
            | Self::CatalogChunks
            | Self::CatalogShardXorbs
            | Self::RunCatalogFiles
            | Self::RunCatalogShards
            | Self::RunCatalogXorbs
            | Self::RunCatalogChunks => 32,
            Self::CommitOids
            | Self::RunAdmission
            | Self::VisibilityChanges
            | Self::CheckpointObjects
            | Self::CheckpointSets => 20,
            #[cfg(feature = "remote-index")]
            Self::PackInventory => 32,
            #[cfg(feature = "remote-index")]
            Self::ObjectLocators => 20,
            #[cfg(feature = "remote-index")]
            Self::PackOffsets => 12,
            _ => 4,
        }
    }
}

pub(crate) type Rows = Vec<(Vec<u8>, Vec<u8>)>;
#[derive(Debug)]
pub(crate) enum Page {
    Leaf(Rows),
    Branch(Vec<PageRef>),
}

impl PageRef {
    pub(crate) fn validate(&self, kind: Kind, source_bytes: u64) -> Result<()> {
        if self.first.len() != kind.key_bytes()
            || self.last.len() != kind.key_bytes()
            || self.first > self.last
            || self.count == 0
            || self.height > MAX_HEIGHT
            || self.bytes as usize > MAX_BYTES
            || (self.bytes as usize) < HEADER
            || self
                .offset
                .checked_add(u64::from(self.bytes))
                .is_none_or(|end| end > source_bytes)
        {
            return Err(corruption("invalid metadata page reference"));
        }
        if kind.is_positional() {
            let first = integer(&self.first)?;
            let last = integer(&self.last)?;
            if last.checked_sub(first).and_then(|span| span.checked_add(1)) != Some(self.count) {
                return Err(corruption("positional page has incomplete key coverage"));
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) bytes: Vec<u8>,
    pages: std::collections::BTreeMap<[u8; 32], PageRef>,
}

impl Writer {
    pub(crate) fn tree(&mut self, kind: Kind, rows: Rows) -> Result<PageRef> {
        validate_rows(kind, &rows)?;
        let mut level = Vec::new();
        let mut pending = Vec::new();
        let mut size = HEADER;
        for row in rows {
            let row_bytes = kind
                .key_bytes()
                .checked_add(4)
                .and_then(|n| n.checked_add(row.1.len()))
                .ok_or_else(|| corruption("metadata row size overflows"))?;
            if row_bytes > MAX_BYTES - HEADER {
                return Err(corruption("metadata row exceeds the page bound"));
            }
            if !pending.is_empty() && size + row_bytes > TARGET_BYTES {
                level.push(self.leaf(kind, std::mem::take(&mut pending))?);
                size = HEADER;
            }
            size += row_bytes;
            pending.push(row);
        }
        if !pending.is_empty() {
            level.push(self.leaf(kind, pending)?);
        }
        let fanout = (TARGET_BYTES - HEADER) / ref_bytes(kind);
        while level.len() > 1 {
            let mut parents = Vec::new();
            for children in level.chunks(fanout) {
                parents.push(self.branch(kind, children)?);
            }
            level = parents;
        }
        level
            .pop()
            .ok_or_else(|| corruption("empty metadata page tree"))
    }

    fn leaf(&mut self, kind: Kind, rows: Rows) -> Result<PageRef> {
        let first = rows
            .first()
            .ok_or_else(|| corruption("empty page"))?
            .0
            .clone();
        let last = rows
            .last()
            .ok_or_else(|| corruption("empty page"))?
            .0
            .clone();
        let count =
            u32::try_from(rows.len()).map_err(|_| corruption("page row count overflows"))?;
        let mut bytes = header(kind, 0, count);
        for (key, value) in rows {
            bytes.extend_from_slice(&key);
            put_u32(&mut bytes, value.len())?;
            bytes.extend_from_slice(&value);
        }
        self.append(bytes, 0, count, first, last)
    }

    fn branch(&mut self, kind: Kind, children: &[PageRef]) -> Result<PageRef> {
        let first = children
            .first()
            .ok_or_else(|| corruption("empty directory"))?;
        let last = children
            .last()
            .ok_or_else(|| corruption("empty directory"))?;
        let height = first
            .height
            .checked_add(1)
            .filter(|height| *height <= MAX_HEIGHT)
            .ok_or_else(|| corruption("page tree is too deep"))?;
        let count = children.iter().try_fold(0u32, |count, child| {
            count
                .checked_add(child.count)
                .ok_or_else(|| corruption("directory count overflows"))
        })?;
        let mut bytes = header(
            kind,
            height,
            u32::try_from(children.len()).map_err(|_| corruption("directory size overflows"))?,
        );
        for child in children {
            encode_ref(&mut bytes, child);
        }
        self.append(bytes, height, count, first.first.clone(), last.last.clone())
    }

    fn append(
        &mut self,
        bytes: Vec<u8>,
        height: u8,
        count: u32,
        first: Vec<u8>,
        last: Vec<u8>,
    ) -> Result<PageRef> {
        if bytes.len() > MAX_BYTES {
            return Err(corruption("page exceeds its byte bound"));
        }
        let hash = *blake3::hash(&bytes).as_bytes();
        if let Some(reference) = self.pages.get(&hash) {
            return Ok(reference.clone());
        }
        let reference = PageRef {
            offset: self.bytes.len() as u64,
            bytes: bytes.len() as u32,
            hash,
            height,
            count,
            first,
            last,
        };
        self.bytes.extend_from_slice(&bytes);
        self.pages.insert(hash, reference.clone());
        Ok(reference)
    }
}

fn header(kind: Kind, height: u8, count: u32) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&[kind as u8, height]);
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes
}

pub(crate) fn decode(
    bytes: &[u8],
    reference: &PageRef,
    kind: Kind,
    source_bytes: u64,
    path: &str,
) -> Result<Page> {
    reference.validate(kind, source_bytes)?;
    if bytes.len() != reference.bytes as usize
        || *blake3::hash(bytes).as_bytes() != reference.hash
        || bytes.get(..8) != Some(MAGIC.as_slice())
        || bytes.get(8) != Some(&(kind as u8))
        || bytes.get(9) != Some(&reference.height)
    {
        return corrupt_at(path, "metadata page identity or header mismatch");
    }
    let mut input = Input::new(&bytes[10..], path);
    let count = input.u32()? as usize;
    if count == 0 {
        return corrupt_at(path, "empty metadata page");
    }
    let page = if reference.height == 0 {
        if count > input.remaining() / (kind.key_bytes() + 4) {
            return corrupt_at(path, "leaf count exceeds its encoded bytes");
        }
        let mut rows = Vec::new();
        for _ in 0..count {
            let key = input.take(kind.key_bytes())?.to_vec();
            let length = input.u32()? as usize;
            let value = input.take(length)?.to_vec();
            rows.push((key, value));
        }
        validate_rows(kind, &rows)?;
        if count != reference.count as usize
            || rows.first().map(|row| &row.0) != Some(&reference.first)
            || rows.last().map(|row| &row.0) != Some(&reference.last)
        {
            return corrupt_at(path, "leaf coverage does not match its reference");
        }
        Page::Leaf(rows)
    } else {
        if count > input.remaining() / ref_bytes(kind) {
            return corrupt_at(path, "directory count exceeds its encoded bytes");
        }
        let mut children: Vec<PageRef> = Vec::new();
        let mut total = 0u32;
        for _ in 0..count {
            let child = decode_ref(&mut input, kind)?;
            child.validate(kind, source_bytes)?;
            if child.height.checked_add(1) != Some(reference.height)
                || child
                    .offset
                    .checked_add(u64::from(child.bytes))
                    .is_none_or(|end| end > reference.offset)
                || children
                    .last()
                    .is_some_and(|previous| previous.last >= child.first)
            {
                return corrupt_at(path, "invalid page directory routing or ancestry");
            }
            if kind.is_positional()
                && children.last().is_some_and(|previous| {
                    integer(&previous.last)
                        .ok()
                        .and_then(|last| last.checked_add(1))
                        != integer(&child.first).ok()
                })
            {
                return corrupt_at(path, "positional directory has a key gap");
            }
            total = total
                .checked_add(child.count)
                .ok_or_else(|| corruption("directory count overflows"))?;
            children.push(child);
        }
        if total != reference.count
            || children.first().map(|child| &child.first) != Some(&reference.first)
            || children.last().map(|child| &child.last) != Some(&reference.last)
        {
            return corrupt_at(path, "directory coverage does not match its reference");
        }
        Page::Branch(children)
    };
    input.finish()?;
    tracing::debug!(target:"crab_metadata::index_decode", index_format=match kind {
        Kind::CommitRecords | Kind::CommitOids | Kind::CommitAncestryRoots | Kind::CommitAncestryNodes => "commit_graph",
        #[cfg(feature = "remote-index")]
        Kind::ObjectLocators | Kind::PackOffsets | Kind::PackInventory | Kind::PackSlots => "git_object_index",
        Kind::RunAdmission | Kind::RunAdmissionCounts => "capsule_admission",
        Kind::VisibilityRefs | Kind::VisibilityChanges => "capsule_visibility",
        Kind::CheckpointObjects | Kind::CheckpointRefs | Kind::CheckpointSets | Kind::CheckpointTransitions => "checkpoint_visibility_pages",
        Kind::CatalogFiles | Kind::CatalogShards | Kind::CatalogXorbs | Kind::CatalogChunks | Kind::CatalogXorbChunks | Kind::CatalogShardXorbs | Kind::RunCatalogFiles | Kind::RunCatalogShards | Kind::RunCatalogXorbs | Kind::RunCatalogChunks => "pointer_catalog_pages",
        _ => "path_state",
    },
        decoded_bytes=bytes.len() as u64, "index page decoded");
    Ok(page)
}

pub(crate) fn scan(
    source: &[u8],
    root: &PageRef,
    kind: Kind,
    path: &str,
    visited: &mut std::collections::BTreeMap<u64, (Kind, PageRef)>,
) -> Result<Rows> {
    let mut pending = vec![root.clone()];
    let mut rows = Vec::new();
    while let Some(reference) = pending.pop() {
        if let Some((previous_kind, previous)) =
            visited.insert(reference.offset, (kind, reference.clone()))
            && (previous_kind != kind || previous != reference)
        {
            return corrupt_at(path, "page has conflicting references");
        }
        let page = page_bytes(source, &reference, path)?;
        match decode(page, &reference, kind, source.len() as u64, path)? {
            Page::Leaf(items) => rows.extend(items),
            Page::Branch(children) => pending.extend(children.into_iter().rev()),
        }
    }
    if rows.len() != root.count as usize {
        return corrupt_at(path, "tree row count mismatch");
    }
    Ok(rows)
}

pub(crate) fn page_bytes<'a>(
    source: &'a [u8],
    reference: &PageRef,
    path: &str,
) -> Result<&'a [u8]> {
    let start = usize::try_from(reference.offset)
        .map_err(|_| corruption("page offset exceeds address space"))?;
    let end = start
        .checked_add(reference.bytes as usize)
        .ok_or_else(|| corruption("page range overflows"))?;
    source
        .get(start..end)
        .ok_or_else(|| crate::validation::corrupt_object(path, "page range is absent"))
}

fn validate_rows(kind: Kind, rows: &Rows) -> Result<()> {
    if rows.is_empty()
        || rows.iter().any(|row| row.0.len() != kind.key_bytes())
        || rows.windows(2).any(|pair| pair[0].0 >= pair[1].0)
    {
        return Err(corruption("page keys are empty, invalid or unordered"));
    }
    if kind.is_positional()
        && rows.windows(2).any(|pair| {
            integer(&pair[0].0)
                .ok()
                .and_then(|value| value.checked_add(1))
                != integer(&pair[1].0).ok()
        })
    {
        return Err(corruption("positional leaf has a key gap"));
    }
    Ok(())
}

fn integer(key: &[u8]) -> Result<u32> {
    let bytes = key
        .try_into()
        .map_err(|_| corruption("invalid ordinal key"))?;
    Ok(u32::from_be_bytes(bytes))
}

fn ref_bytes(kind: Kind) -> usize {
    49 + 2 * kind.key_bytes()
}

pub(crate) fn encode_ref(bytes: &mut Vec<u8>, reference: &PageRef) {
    bytes.extend_from_slice(&reference.offset.to_le_bytes());
    bytes.extend_from_slice(&reference.bytes.to_le_bytes());
    bytes.extend_from_slice(&reference.hash);
    bytes.push(reference.height);
    bytes.extend_from_slice(&reference.count.to_le_bytes());
    bytes.extend_from_slice(&reference.first);
    bytes.extend_from_slice(&reference.last);
}

pub(crate) fn decode_ref(input: &mut Input<'_, '_>, kind: Kind) -> Result<PageRef> {
    Ok(PageRef {
        offset: u64::from_le_bytes(input.array()?),
        bytes: input.u32()?,
        hash: input.array()?,
        height: input.array::<1>()?[0],
        count: input.u32()?,
        first: input.take(kind.key_bytes())?.to_vec(),
        last: input.take(kind.key_bytes())?.to_vec(),
    })
}

pub(crate) fn put_u32(bytes: &mut Vec<u8>, value: usize) -> Result<()> {
    bytes.extend_from_slice(
        &u32::try_from(value)
            .map_err(|_| corruption("page scalar overflows"))?
            .to_le_bytes(),
    );
    Ok(())
}

pub(crate) struct Input<'a, 'p> {
    bytes: &'a [u8],
    path: &'p str,
}
impl<'a, 'p> Input<'a, 'p> {
    pub(crate) fn new(bytes: &'a [u8], path: &'p str) -> Self {
        Self { bytes, path }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let (head, tail) = self
            .bytes
            .split_at_checked(count)
            .ok_or_else(|| crate::validation::corrupt_object(self.path, "truncated page row"))?;
        self.bytes = tail;
        Ok(head)
    }
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| corruption("invalid page scalar"))
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub(crate) fn finish(self) -> Result<()> {
        if !self.bytes.is_empty() {
            return corrupt_at(self.path, "page has trailing bytes");
        }
        Ok(())
    }
}

#[cfg(feature = "storage")]
impl Page {
    pub(crate) fn resident_bytes(&self) -> usize {
        match self {
            Self::Leaf(rows) => rows.iter().fold(
                rows.capacity()
                    .saturating_mul(std::mem::size_of::<(Vec<u8>, Vec<u8>)>()),
                |total, (key, value)| {
                    total
                        .saturating_add(key.capacity())
                        .saturating_add(value.capacity())
                },
            ),
            Self::Branch(children) => children.iter().fold(
                children
                    .capacity()
                    .saturating_mul(std::mem::size_of::<PageRef>()),
                |total, child| {
                    total
                        .saturating_add(child.first.capacity())
                        .saturating_add(child.last.capacity())
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: u32, value_bytes: usize) -> Rows {
        (0..count)
            .map(|index| (index.to_be_bytes().to_vec(), vec![7; value_bytes]))
            .collect()
    }

    #[test]
    fn large_trees_page_their_internal_directories() {
        let expected = rows(20_000, 1000);
        let mut writer = Writer::default();
        let root = writer.tree(Kind::Records, expected.clone()).unwrap();
        let actual = scan(
            &writer.bytes,
            &root,
            Kind::Records,
            "tree",
            &mut Default::default(),
        )
        .unwrap();
        assert_eq!(
            (
                root.height >= 2,
                writer
                    .pages
                    .values()
                    .all(|page| page.bytes as usize <= MAX_BYTES),
                actual == expected
            ),
            (true, true, true)
        );
    }

    #[test]
    fn authenticated_directories_reject_overlapping_routing() {
        let mut writer = Writer::default();
        let mut root = writer.tree(Kind::Records, rows(100, 1000)).unwrap();
        let mut body = page_bytes(&writer.bytes, &root, "tree").unwrap().to_vec();
        let first = body[HEADER..HEADER + ref_bytes(Kind::Records)].to_vec();
        body[HEADER + ref_bytes(Kind::Records)..HEADER + 2 * ref_bytes(Kind::Records)]
            .copy_from_slice(&first);
        root.hash = *blake3::hash(&body).as_bytes();
        assert!(
            decode(
                &body,
                &root,
                Kind::Records,
                writer.bytes.len() as u64,
                "tree"
            )
            .is_err()
        );
    }

    #[test]
    fn page_counts_cannot_allocate_beyond_encoded_bytes() {
        let mut writer = Writer::default();
        let mut root = writer.tree(Kind::Records, rows(3, 48)).unwrap();
        let mut body = page_bytes(&writer.bytes, &root, "tree").unwrap().to_vec();
        body[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        root.hash = *blake3::hash(&body).as_bytes();
        assert!(
            decode(
                &body,
                &root,
                Kind::Records,
                writer.bytes.len() as u64,
                "tree"
            )
            .is_err()
        );
    }

    #[test]
    fn page_hash_lengths_and_bounds_are_checked_before_decoding() {
        let mut writer = Writer::default();
        let root = writer.tree(Kind::Records, rows(3, 48)).unwrap();
        let body = page_bytes(&writer.bytes, &root, "tree").unwrap().to_vec();
        let mut invalid = root.clone();
        invalid.first = 1u32.to_be_bytes().to_vec();
        let mut damaged = body.clone();
        damaged[HEADER] ^= 1;
        let oversized = PageRef {
            bytes: (MAX_BYTES + 1) as u32,
            ..root.clone()
        };
        assert_eq!(
            (
                decode(
                    &body,
                    &invalid,
                    Kind::Records,
                    writer.bytes.len() as u64,
                    "tree"
                )
                .is_err(),
                decode(
                    &damaged,
                    &root,
                    Kind::Records,
                    writer.bytes.len() as u64,
                    "tree"
                )
                .is_err(),
                oversized.validate(Kind::Records, u64::MAX).is_err()
            ),
            (true, true, true)
        );
    }

    proptest::proptest! {
        #[test]
        fn ordered_page_trees_round_trip(count in 1u32..1000, value in proptest::collection::vec(proptest::num::u8::ANY, 0..300)) {
            let expected = (0..count).map(|index| (index.to_be_bytes().to_vec(), value.clone())).collect::<Rows>();
            let mut writer = Writer::default();
            let root = writer.tree(Kind::Records, expected.clone()).unwrap();
            let actual = scan(&writer.bytes, &root, Kind::Records, "tree", &mut Default::default()).unwrap();
            proptest::prop_assert_eq!(actual, expected);
        }
    }
}
