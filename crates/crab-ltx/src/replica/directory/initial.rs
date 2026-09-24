use crate::{CellObjectKind, CrabError, Result};

use super::super::merge::LocatorMerge;
use super::super::{CellReplica, DirectoryInput, OBJECT_UPLOAD_CONCURRENCY};
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
    merge: LocatorMerge,
}

pub(in crate::replica) fn entries(inputs: &[DirectoryInput]) -> Result<Entries<'_>> {
    if inputs.is_empty() {
        return Err(CrabError::LTXCorrupted);
    }
    let mut merge = LocatorMerge::new(
        inputs
            .iter()
            .map(|input| input.descriptor.info.database_pages),
    );
    let mut cursors = Vec::with_capacity(inputs.len());
    for input in inputs {
        let cursor = IndexCursor::new(input)?;
        if let Some(entry) = &cursor.current {
            merge.push(cursors.len(), entry.page);
        }
        cursors.push(cursor);
    }
    Ok(Entries { cursors, merge })
}

impl Iterator for Entries<'_> {
    type Item = Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let Self { cursors, merge } = self;
        merge.next_locator(|index| {
            let cursor = cursors.get_mut(index).ok_or(CrabError::LTXCorrupted)?;
            let entry = cursor.current.take().ok_or(CrabError::LTXCorrupted)?;
            cursor.advance()?;
            Ok((entry, cursor.current.as_ref().map(|entry| entry.page)))
        })
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
    let mut pending = Vec::with_capacity(OBJECT_UPLOAD_CONCURRENCY);
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
            pending.push(encode_leaf_node(leaf_index, &leaf_entries)?);
            if pending.len() == OBJECT_UPLOAD_CONCURRENCY {
                flush_node_uploads(replica, &mut pending, &mut nodes).await?;
            }
            leaf_entries.clear();
        }
        leaf_index = Some(index);
        leaf_entries.push(entry);
        live_pages += 1;
        expected_page += 1;
    }
    if !leaf_entries.is_empty() {
        pending.push(encode_leaf_node(leaf_index, &leaf_entries)?);
    }
    flush_node_uploads(replica, &mut pending, &mut nodes).await?;
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
        let mut pending = Vec::with_capacity(OBJECT_UPLOAD_CONCURRENCY);
        for group in
            nodes.chunk_by(|left, right| left.index / FANOUT as u32 == right.index / FANOUT as u32)
        {
            let index = group[0].index / FANOUT as u32;
            let bytes = encode_branch(group)?;
            let node = Node::from_bytes(index, &bytes)?;
            pending.push((node, bytes));
            if pending.len() == OBJECT_UPLOAD_CONCURRENCY {
                flush_node_uploads(replica, &mut pending, &mut parents).await?;
            }
        }
        flush_node_uploads(replica, &mut pending, &mut parents).await?;
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

fn encode_leaf_node(index: Option<u32>, entries: &[DirectoryEntry]) -> Result<(Node, Vec<u8>)> {
    let index = index.ok_or(CrabError::LTXCorrupted)?;
    let bytes = encode_leaf(entries)?;
    let node = Node::from_bytes(index, &bytes)?;
    Ok((node, bytes))
}

async fn flush_node_uploads(
    replica: &CellReplica,
    pending: &mut Vec<(Node, Vec<u8>)>,
    output: &mut Vec<Node>,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let pending = std::mem::replace(pending, Vec::with_capacity(OBJECT_UPLOAD_CONCURRENCY));
    let mut nodes = Vec::with_capacity(pending.len());
    let objects = pending
        .into_iter()
        .map(|(node, bytes)| {
            let digest = node.digest;
            nodes.push(node);
            (digest, bytes)
        })
        .collect();
    replica
        .put_objects(CellObjectKind::Directory, objects)
        .await?;
    output.extend(nodes);
    Ok(())
}
