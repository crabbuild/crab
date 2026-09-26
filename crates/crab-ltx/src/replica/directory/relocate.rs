//! Stream representation-only locator changes through an authenticated directory.

use std::{collections::BTreeMap, future::Future, pin::Pin};

use futures_util::{Stream, TryStreamExt as _};

use super::super::{
    CellReplica, LoadedGraph, OBJECT_UPLOAD_CONCURRENCY, SegmentDescriptor, object_extents,
};
use super::{
    DirectoryEntry, DirectoryTree, Header, Node, ObjectExtent, Verification, encode_branch,
    encode_leaf, read_node, verify_branch, verify_leaf,
};
use crate::{CellObjectKind, CrabError, Result};

pub(in crate::replica) async fn run(
    replica: &CellReplica,
    graph: &LoadedGraph,
    descriptors: &[SegmentDescriptor],
    selected: &[SegmentDescriptor],
    entries: impl Stream<Item = Result<DirectoryEntry>> + Send,
) -> Result<DirectoryTree> {
    let base_extents = object_extents(&graph.descriptors)?;
    let final_extents = object_extents(descriptors)?;
    let selected_extents = object_extents(selected)?;
    let base = Verification {
        layout: &replica.layout,
        cell: &replica.cell,
        incarnation: &replica.incarnation,
        page_size: graph.document.page_size,
        database_pages: graph.document.database_pages,
        extents: &base_extents,
        host: &replica.host,
        origin: crate::LtxReadOrigin::Cold,
    };
    let mut state = Relocator {
        replica,
        base,
        final_extents: &final_extents,
        selected_extents: &selected_extents,
        entries: Box::pin(entries),
        next: None,
        previous: 0,
        pending: Vec::with_capacity(OBJECT_UPLOAD_CONCURRENCY),
    };
    state.advance().await?;
    let root = Node {
        index: 0,
        digest: graph.document.directory_digest,
        aggregate: graph.aggregate,
    };
    let root = state.rewrite(root, graph.document.directory_height).await?;
    if state.next.is_some() || root.aggregate != graph.aggregate {
        return Err(CrabError::ChecksumMismatch);
    }
    state.flush().await?;
    Ok(DirectoryTree {
        objects: Vec::new(),
        root,
        height: graph.document.directory_height,
    })
}

struct Relocator<'a> {
    replica: &'a CellReplica,
    base: Verification<'a>,
    final_extents: &'a BTreeMap<[u8; 32], ObjectExtent>,
    selected_extents: &'a BTreeMap<[u8; 32], ObjectExtent>,
    entries: Pin<Box<dyn Stream<Item = Result<DirectoryEntry>> + Send + 'a>>,
    next: Option<DirectoryEntry>,
    previous: u32,
    pending: Vec<([u8; 32], Vec<u8>)>,
}

impl Relocator<'_> {
    async fn advance(&mut self) -> Result<()> {
        self.next = None;
        while let Some(entry) = self.entries.try_next().await? {
            if entry.page <= self.previous
                || entry.page == crate::ltx::lock_pgno(self.base.page_size)
            {
                return Err(CrabError::LTXCorrupted);
            }
            self.previous = entry.page;
            // Later cuts may truncate pages retained at the selected range's
            // endpoint. Their compacted bytes must not revive final-root pages.
            if entry.page <= self.base.database_pages {
                self.next = Some(entry);
                break;
            }
        }
        Ok(())
    }

    fn rewrite(
        &mut self,
        old: Node,
        level: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Node>> + Send + '_>> {
        Box::pin(async move {
            let Some(next) = &self.next else {
                return Ok(old);
            };
            if next.page > old.aggregate.last {
                return Ok(old);
            }
            if next.page < old.aggregate.first || level > 3 {
                return Err(CrabError::LTXCorrupted);
            }
            let bytes = read_node(&self.base, old.digest).await?;
            let header = Header::parse(&bytes)?;
            if (level == 0) != (header.kind == 0) {
                return Err(CrabError::LTXCorrupted);
            }
            let bytes = if level == 0 {
                self.leaf(&old, &bytes, &header).await?
            } else {
                let (aggregate, mut children) = verify_branch(&bytes, &header)?;
                if aggregate != old.aggregate {
                    return Err(CrabError::ChecksumMismatch);
                }
                for child in &mut children {
                    *child = self.rewrite(child.clone(), level - 1).await?;
                }
                encode_branch(&children)?
            };
            let node = Node::from_bytes(old.index, &bytes)?;
            if node.aggregate != old.aggregate {
                return Err(CrabError::ChecksumMismatch);
            }
            if node.digest != old.digest {
                self.pending.push((node.digest, bytes));
                if self.pending.len() == OBJECT_UPLOAD_CONCURRENCY {
                    self.flush().await?;
                }
            }
            Ok(node)
        })
    }

    async fn leaf(&mut self, old: &Node, bytes: &[u8], header: &Header) -> Result<Vec<u8>> {
        let (aggregate, mut entries) = verify_leaf(
            bytes,
            header,
            self.base.page_size,
            self.base.database_pages,
            self.base.extents,
        )?;
        if aggregate != old.aggregate {
            return Err(CrabError::ChecksumMismatch);
        }
        for entry in &mut entries {
            let Some(next) = &self.next else {
                break;
            };
            if next.page < entry.page {
                return Err(CrabError::LTXCorrupted);
            }
            if next.page != entry.page {
                continue;
            }
            // Object identity alone is insufficient: selected and newer cuts
            // may occupy disjoint ranges of the same follower bundle.
            let selected = self
                .selected_extents
                .get(&entry.object)
                .is_some_and(|extent| {
                    extent.ranges.iter().any(|range| {
                        range.start <= entry.offset
                            && entry.offset + u64::from(entry.length) <= range.end
                    })
                });
            if selected {
                if entry.checksum != next.checksum {
                    return Err(CrabError::ChecksumMismatch);
                }
                *entry = next.clone();
            }
            self.advance().await?;
        }
        let bytes = encode_leaf(&entries)?;
        let header = Header::parse(&bytes)?;
        verify_leaf(
            &bytes,
            &header,
            self.base.page_size,
            self.base.database_pages,
            self.final_extents,
        )?;
        Ok(bytes)
    }

    async fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let objects = std::mem::replace(
            &mut self.pending,
            Vec::with_capacity(OBJECT_UPLOAD_CONCURRENCY),
        );
        // Only immutable dependencies escape here. The owner cannot publish
        // a proposal until every changed branch and its children have uploaded.
        self.replica
            .put_objects(CellObjectKind::Directory, objects)
            .await
    }
}
