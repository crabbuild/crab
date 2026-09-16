use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
};

use crate::{CrabError, Result};

use super::{
    Aggregate, DirectoryEntry, DirectoryObject, DirectoryTree, FANOUT, Header, Node, Verification,
    encode_branch, encode_leaf, read_node, verify_branch, verify_leaf,
};

type ChangedLeaves = BTreeMap<u32, Vec<DirectoryEntry>>;

pub(super) async fn run(
    base: Verification<'_>,
    root: [u8; 32],
    height: u32,
    aggregate: Aggregate,
    changes: BTreeMap<u32, DirectoryEntry>,
    retain_through: u32,
    final_state: Verification<'_>,
    expected_checksum: u64,
) -> Result<DirectoryTree> {
    if height > 3
        || base.page_size != final_state.page_size
        || retain_through > base.database_pages
        || final_state.database_pages == 0
    {
        return Err(CrabError::LTXCorrupted);
    }
    let changed = group_changes(changes, &final_state)?;
    let mut objects = Vec::new();
    let old = Node {
        index: 0,
        digest: root,
        aggregate,
    };
    let root_span = node_span(height)?;
    let root_end = u32::try_from(root_span).map_err(|_| CrabError::LTXCorrupted)?;
    let root_changes = changed
        .range(..root_end)
        .map(|(leaf, entries)| (*leaf, entries.clone()))
        .collect::<ChangedLeaves>();
    let mut nodes = Vec::new();
    if let Some(node) = mutate_node(
        base,
        final_state,
        Some(old),
        height,
        0,
        &root_changes,
        retain_through,
        &mut objects,
    )
    .await?
    {
        nodes.push(node);
    }

    let mut outside = changed.range(root_end..).peekable();
    while let Some((leaf, _)) = outside.peek() {
        let node_index = u64::from(**leaf) / root_span;
        let node_index = u32::try_from(node_index).map_err(|_| CrabError::LTXCorrupted)?;
        let end = u64::from(node_index)
            .checked_add(1)
            .and_then(|value| value.checked_mul(root_span))
            .ok_or(CrabError::LTXCorrupted)?;
        let mut subtree = ChangedLeaves::new();
        while let Some((leaf, entries)) = outside.peek() {
            if u64::from(**leaf) >= end {
                break;
            }
            subtree.insert(**leaf, (*entries).clone());
            outside.next();
        }
        let node = mutate_node(
            base,
            final_state,
            None,
            height,
            node_index,
            &subtree,
            retain_through,
            &mut objects,
        )
        .await?
        .ok_or(CrabError::LTXCorrupted)?;
        nodes.push(node);
    }

    let mut final_height = height;
    while nodes.len() > 1 {
        nodes = build_parent_level(nodes, &mut objects)?;
        final_height = final_height
            .checked_add(1)
            .filter(|height| *height <= 3)
            .ok_or(CrabError::LTXCorrupted)?;
    }
    let root = nodes.pop().ok_or(CrabError::LTXCorrupted)?;
    let lock = crate::ltx::lock_pgno(final_state.page_size);
    let expected_pages =
        u64::from(final_state.database_pages) - u64::from(lock <= final_state.database_pages);
    if root.index != 0
        || root.aggregate.live_pages != expected_pages
        || root.aggregate.checksum != expected_checksum
        || root.aggregate.first == 0
        || root.aggregate.last > final_state.database_pages
    {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok(DirectoryTree {
        objects,
        root,
        height: final_height,
    })
}

fn mutate_node<'a>(
    base: Verification<'a>,
    final_state: Verification<'a>,
    old: Option<Node>,
    level: u32,
    index: u32,
    changes: &'a ChangedLeaves,
    retain_through: u32,
    objects: &'a mut Vec<DirectoryObject>,
) -> Pin<Box<dyn Future<Output = Result<Option<Node>>> + Send + 'a>> {
    Box::pin(async move {
        if old
            .as_ref()
            .is_some_and(|node| node.aggregate.last <= retain_through && changes.is_empty())
        {
            return Ok(old);
        }
        if old
            .as_ref()
            .is_some_and(|node| node.aggregate.first > retain_through && changes.is_empty())
        {
            // Authenticated child ranges let truncation discard whole subtrees
            // without loading their leaves or reviving any retired locator.
            return Ok(None);
        }
        if old.is_none() && changes.is_empty() {
            return Ok(None);
        }
        if level == 0 {
            return mutate_leaf(
                base,
                final_state,
                old,
                index,
                changes,
                retain_through,
                objects,
            )
            .await;
        }

        let mut children = BTreeMap::new();
        if let Some(node) = &old {
            let bytes = read_node(&base, node.digest).await?;
            let header = Header::parse(&bytes)?;
            if header.kind != 1 {
                return Err(CrabError::LTXCorrupted);
            }
            let (actual, decoded) = verify_branch(&bytes, &header)?;
            if actual != node.aggregate {
                return Err(CrabError::ChecksumMismatch);
            }
            for child in decoded {
                let child_index = index_for_page(child.aggregate.first, level - 1)?;
                if index_for_page(child.aggregate.last, level - 1)? != child_index
                    || child_index / FANOUT as u32 != index
                    || children.insert(child_index, child).is_some()
                {
                    return Err(CrabError::LTXCorrupted);
                }
            }
        }

        let mut selected = children.keys().copied().collect::<BTreeSet<_>>();
        for leaf in changes.keys() {
            selected.insert(node_index_for_leaf(*leaf, level - 1)?);
        }
        let mut output = Vec::with_capacity(selected.len());
        for child_index in selected {
            let old_child = children.remove(&child_index);
            let subtree = changes_for_node(changes, child_index, level - 1)?;
            let affected = !subtree.is_empty()
                || old_child
                    .as_ref()
                    .is_some_and(|child| child.aggregate.last > retain_through);
            if !affected {
                if let Some(child) = old_child {
                    output.push(child);
                }
                continue;
            }
            if let Some(child) = mutate_node(
                base,
                final_state,
                old_child,
                level - 1,
                child_index,
                &subtree,
                retain_through,
                objects,
            )
            .await?
            {
                output.push(child);
            }
        }
        if output.is_empty() {
            return Ok(None);
        }
        output.sort_by_key(|child| child.index);
        let bytes = encode_branch(&output)?;
        let node = Node::from_bytes(index, &bytes)?;
        if old.as_ref().is_some_and(|old| old.digest == node.digest) {
            return Ok(old);
        }
        objects.push(DirectoryObject {
            digest: node.digest,
            bytes,
        });
        Ok(Some(node))
    })
}

async fn mutate_leaf(
    base: Verification<'_>,
    final_state: Verification<'_>,
    old: Option<Node>,
    index: u32,
    changes: &ChangedLeaves,
    retain_through: u32,
    objects: &mut Vec<DirectoryObject>,
) -> Result<Option<Node>> {
    let mut entries = BTreeMap::new();
    if let Some(node) = &old {
        let bytes = read_node(&base, node.digest).await?;
        let header = Header::parse(&bytes)?;
        if header.kind != 0 {
            return Err(CrabError::LTXCorrupted);
        }
        let (actual, decoded) = verify_leaf(
            &bytes,
            &header,
            base.page_size,
            base.database_pages,
            base.extents,
        )?;
        if actual != node.aggregate {
            return Err(CrabError::ChecksumMismatch);
        }
        entries.extend(
            decoded
                .into_iter()
                .filter(|entry| entry.page <= retain_through)
                .map(|entry| (entry.page, entry)),
        );
    }
    if let Some(changed) = changes.get(&index) {
        entries.extend(changed.iter().cloned().map(|entry| (entry.page, entry)));
    }
    entries.retain(|page, _| *page <= final_state.database_pages);
    if entries.is_empty() {
        return Ok(None);
    }
    let entries = entries.into_values().collect::<Vec<_>>();
    let bytes = encode_leaf(&entries)?;
    let header = Header::parse(&bytes)?;
    verify_leaf(
        &bytes,
        &header,
        final_state.page_size,
        final_state.database_pages,
        final_state.extents,
    )?;
    let node = Node::from_bytes(index, &bytes)?;
    if old.as_ref().is_some_and(|old| old.digest == node.digest) {
        return Ok(old);
    }
    objects.push(DirectoryObject {
        digest: node.digest,
        bytes,
    });
    Ok(Some(node))
}

fn group_changes(
    changes: BTreeMap<u32, DirectoryEntry>,
    final_state: &Verification<'_>,
) -> Result<ChangedLeaves> {
    let lock = crate::ltx::lock_pgno(final_state.page_size);
    let mut leaves = ChangedLeaves::new();
    for (page, entry) in changes {
        if page != entry.page || page == 0 || page > final_state.database_pages || page == lock {
            return Err(CrabError::LTXCorrupted);
        }
        leaves
            .entry((page - 1) / FANOUT as u32)
            .or_default()
            .push(entry);
    }
    Ok(leaves)
}

fn changes_for_node(changes: &ChangedLeaves, index: u32, level: u32) -> Result<ChangedLeaves> {
    let span = node_span(level)?;
    let first = u64::from(index)
        .checked_mul(span)
        .ok_or(CrabError::LTXCorrupted)?;
    let end = first.checked_add(span).ok_or(CrabError::LTXCorrupted)?;
    let first = u32::try_from(first).map_err(|_| CrabError::LTXCorrupted)?;
    let end = u32::try_from(end).map_err(|_| CrabError::LTXCorrupted)?;
    Ok(changes
        .range(first..end)
        .map(|(leaf, entries)| (*leaf, entries.clone()))
        .collect())
}

fn build_parent_level(nodes: Vec<Node>, objects: &mut Vec<DirectoryObject>) -> Result<Vec<Node>> {
    let mut parents = Vec::new();
    for group in
        nodes.chunk_by(|left, right| left.index / FANOUT as u32 == right.index / FANOUT as u32)
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
    Ok(parents)
}

fn index_for_page(page: u32, level: u32) -> Result<u32> {
    if page == 0 {
        return Err(CrabError::LTXCorrupted);
    }
    node_index_for_leaf((page - 1) / FANOUT as u32, level)
}

fn node_index_for_leaf(leaf: u32, level: u32) -> Result<u32> {
    let span = node_span(level)?;
    u32::try_from(u64::from(leaf) / span).map_err(|_| CrabError::LTXCorrupted)
}

fn node_span(level: u32) -> Result<u64> {
    (0..level).try_fold(1_u64, |span, _| {
        span.checked_mul(FANOUT as u64)
            .ok_or(CrabError::LTXCorrupted)
    })
}
