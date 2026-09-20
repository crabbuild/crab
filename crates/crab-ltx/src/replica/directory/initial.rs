use std::{cmp::Reverse, collections::BinaryHeap};

use crab_storage::CellObjectKind;

use crate::{CrabError, Result};

use super::super::{CellReplica, DirectoryInput};
use super::{DirectoryEntry, DirectoryTree, FANOUT, Node, encode_branch, encode_leaf};

struct IndexCursor<'a> {
    input: &'a DirectoryInput,
    entries: crate::paged::ValidatedIndexEntries<'a>,
    current: Option<DirectoryEntry>,
}

impl<'a> IndexCursor<'a> {
    fn new(input: &'a DirectoryInput) -> Result<Self> {
        let mut cursor = Self {
            input,
            entries: crate::paged::validated_index_entries(&input.index, &input.descriptor.info)?,
            current: None,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    fn advance(&mut self) -> Result<()> {
        let Some(entry) = self.entries.next().transpose()? else {
            self.current = None;
            return Ok(());
        };
        let descriptor = &self.input.descriptor;
        self.current = Some(DirectoryEntry {
            page: entry.page,
            object: descriptor.object_digest(),
            offset: descriptor
                .offset()
                .checked_add(entry.offset)
                .ok_or(CrabError::LTXCorrupted)?,
            length: u32::try_from(entry.size).map_err(|_| CrabError::LTXCorrupted)?,
            frame_hash: entry.hash,
            checksum: entry.checksum,
        });
        Ok(())
    }
}

pub(in crate::replica) struct Entries<'a> {
    cursors: Vec<IndexCursor<'a>>,
    heap: BinaryHeap<Reverse<(u32, usize)>>,
    valid_through: Vec<u32>,
    failed: bool,
}

pub(in crate::replica) fn entries(inputs: &[DirectoryInput]) -> Result<Entries<'_>> {
    if inputs.is_empty() {
        return Err(CrabError::LTXCorrupted);
    }
    // A later truncation invalidates every older locator above its boundary.
    // Suffix minima let the merge discard those locators without a page map.
    let mut valid_through = vec![0; inputs.len()];
    let mut suffix_min = u32::MAX;
    for (index, input) in inputs.iter().enumerate().rev() {
        suffix_min = suffix_min.min(input.descriptor.info.database_pages);
        valid_through[index] = suffix_min;
    }
    let mut cursors = Vec::with_capacity(inputs.len());
    let mut heap = BinaryHeap::new();
    for input in inputs {
        let cursor = IndexCursor::new(input)?;
        let index = cursors.len();
        if let Some(entry) = &cursor.current {
            heap.push(Reverse((entry.page, index)));
        }
        cursors.push(cursor);
    }
    Ok(Entries {
        cursors,
        heap,
        valid_through,
        failed: false,
    })
}

impl Entries<'_> {
    fn take_current(&mut self, index: usize) -> Result<DirectoryEntry> {
        let cursor = self.cursors.get_mut(index).ok_or(CrabError::LTXCorrupted)?;
        let entry = cursor.current.take().ok_or(CrabError::LTXCorrupted)?;
        cursor.advance()?;
        if let Some(next) = &cursor.current {
            self.heap.push(Reverse((next.page, index)));
        }
        Ok(entry)
    }
}

impl Iterator for Entries<'_> {
    type Item = Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            let Reverse((page, first_index)) = self.heap.pop()?;
            let mut selected = None;
            let mut index = first_index;
            loop {
                let entry = match self.take_current(index) {
                    Ok(entry) => entry,
                    Err(error) => {
                        self.failed = true;
                        self.heap.clear();
                        return Some(Err(error));
                    }
                };
                if page <= self.valid_through[index]
                    && selected
                        .as_ref()
                        .is_none_or(|(selected_index, _)| index > *selected_index)
                {
                    // The newest surviving cut is the authoritative locator.
                    selected = Some((index, entry));
                }
                let Some(Reverse((next_page, next_index))) = self.heap.peek().copied() else {
                    break;
                };
                if next_page != page {
                    break;
                }
                self.heap.pop();
                index = next_index;
            }
            if let Some((_, entry)) = selected {
                return Some(Ok(entry));
            }
        }
    }
}

pub(in crate::replica) async fn build_and_upload(
    entries: impl Iterator<Item = Result<DirectoryEntry>>,
    page_size: u32,
    database_pages: u32,
    replica: &CellReplica,
) -> Result<DirectoryTree> {
    if database_pages == 0 {
        return Err(CrabError::LTXCorrupted);
    }
    let lock = crate::ltx::lock_pgno(page_size);
    let mut expected_page = 1_u64;
    let mut live_pages = 0_u64;
    let mut leaf_index = None;
    let mut leaf_entries = Vec::with_capacity(FANOUT);
    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry?;
        if expected_page == u64::from(lock) {
            expected_page += 1;
        }
        if u64::from(entry.page) != expected_page
            || entry.page > database_pages
            || entry.page == lock
        {
            return Err(CrabError::LTXCorrupted);
        }
        let index = (entry.page - 1) / FANOUT as u32;
        if leaf_index.is_some_and(|current| current != index) {
            // Directory nodes are immutable and the private PreparedRoot is
            // unreachable until every dependency is uploaded and verified.
            nodes.push(upload_leaf(replica, leaf_index, &leaf_entries).await?);
            leaf_entries.clear();
        }
        leaf_index = Some(index);
        leaf_entries.push(entry);
        live_pages += 1;
        expected_page += 1;
    }
    if !leaf_entries.is_empty() {
        nodes.push(upload_leaf(replica, leaf_index, &leaf_entries).await?);
    }
    if expected_page == u64::from(lock) {
        expected_page += 1;
    }
    let expected_live = u64::from(database_pages) - u64::from(lock <= database_pages);
    if expected_page != u64::from(database_pages) + 1 || live_pages != expected_live {
        return Err(CrabError::LTXCorrupted);
    }

    let mut height = 0;
    while nodes.len() > 1 {
        let mut parents = Vec::new();
        for group in
            nodes.chunk_by(|left, right| left.index / FANOUT as u32 == right.index / FANOUT as u32)
        {
            let index = group[0].index / FANOUT as u32;
            let bytes = encode_branch(group)?;
            let node = Node::from_bytes(index, &bytes)?;
            replica
                .put_object(&node.digest, CellObjectKind::Directory, bytes)
                .await?;
            parents.push(node);
        }
        nodes = parents;
        height += 1;
    }
    let root = nodes.pop().ok_or(CrabError::LTXCorrupted)?;
    Ok(DirectoryTree {
        objects: Vec::new(),
        root,
        height,
    })
}

async fn upload_leaf(
    replica: &CellReplica,
    index: Option<u32>,
    entries: &[DirectoryEntry],
) -> Result<Node> {
    let index = index.ok_or(CrabError::LTXCorrupted)?;
    let bytes = encode_leaf(entries)?;
    let node = Node::from_bytes(index, &bytes)?;
    replica
        .put_object(&node.digest, CellObjectKind::Directory, bytes)
        .await?;
    Ok(node)
}
