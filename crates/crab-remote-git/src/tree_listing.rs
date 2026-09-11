use std::cmp::Ordering;
use std::sync::Arc;

use bytes::Bytes;
use gix_hash::ObjectId;

use crate::objects::{RawTreeEntry, tree_name_cmp};
use crate::{
    BudgetDimension, EntryKind, Error, GitPath, OperationContext, RemoteGitSnapshot, Result,
    TreeEntry,
};

/// One projected item in a bounded recursive blob listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeListingItem {
    /// One ordinary or executable Git blob.
    Blob(TreeEntry),
    /// One grouped path prefix ending in the requested delimiter.
    CommonPrefix(Bytes),
}

impl TreeListingItem {
    /// Return the exact repository-relative path represented by this item.
    #[must_use]
    pub fn path(&self) -> &[u8] {
        match self {
            Self::Blob(entry) => entry.path.as_bytes(),
            Self::CommonPrefix(prefix) => prefix,
        }
    }
}

/// One bounded recursive blob-listing page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeListingPage {
    /// Items in canonical recursive Git path order.
    pub items: Vec<TreeListingItem>,
    /// Whether at least one matching item follows this page.
    pub has_more: bool,
}

/// Prefix, continuation, grouping, and result bound for a recursive blob listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeListingRequest {
    prefix: Bytes,
    after: Option<Bytes>,
    delimiter: Option<u8>,
    limit: usize,
}

impl TreeListingRequest {
    /// Construct a bounded listing request over exact Git path bytes.
    pub fn new(
        prefix: impl Into<Bytes>,
        after: Option<Bytes>,
        delimiter: Option<u8>,
        limit: usize,
    ) -> Result<Self> {
        if limit == 0 || limit.checked_add(1).is_none() {
            return Err(Error::InvalidLimit {
                name: "tree listing result limit",
            });
        }
        Ok(Self {
            prefix: prefix.into(),
            after,
            delimiter,
            limit,
        })
    }
}

struct TreeFrame {
    entries: Arc<Vec<RawTreeEntry>>,
    parent: GitPath,
    next: usize,
    depth: u64,
}

impl RemoteGitSnapshot {
    /// Return one bounded page of blobs or grouped prefixes in raw path order.
    ///
    /// Traversal seeks within each visited tree and stops after one lookahead item.
    /// Blob bodies are never read. The continuation is an exclusive raw path bound;
    /// callers exposing it outside the process remain responsible for authorization.
    pub async fn list_tree_blobs(
        &self,
        request: &TreeListingRequest,
        operation: &OperationContext,
    ) -> Result<TreeListingPage> {
        if !operation.belongs_to(&self.repository) {
            return Err(Error::InternalInvariant {
                invariant: "operation belongs to another repository generation",
            });
        }
        operation.ensure_active()?;
        let upper = prefix_successor(&request.prefix);
        if request
            .after
            .as_deref()
            .is_some_and(|after| upper.as_deref().is_some_and(|upper| after >= upper))
        {
            return Ok(TreeListingPage {
                items: Vec::new(),
                has_more: false,
            });
        }
        let lower = request
            .after
            .as_deref()
            .filter(|after| *after > request.prefix.as_ref())
            .unwrap_or(&request.prefix);
        let root = open_frame(operation, self.root_tree_oid, GitPath::root(), 0, lower).await?;
        let mut frames = vec![root];
        let capacity = request.limit.checked_add(1).ok_or(Error::InvalidLimit {
            name: "tree listing result limit",
        })?;
        let mut items = Vec::new();
        items
            .try_reserve_exact(capacity)
            .map_err(|source| Error::Allocation {
                requested: capacity.saturating_mul(std::mem::size_of::<TreeListingItem>()),
                source,
            })?;

        while !frames.is_empty() && items.len() < capacity {
            operation.ensure_active()?;
            let Some((raw, parent, depth)) = next_entry(&mut frames) else {
                continue;
            };
            let entry_depth = depth.checked_add(1).ok_or(Error::LimitExceeded {
                limit: "depth",
                actual: u64::MAX,
                maximum: u64::MAX,
            })?;
            operation
                .charge(BudgetDimension::Depth, entry_depth)
                .await?;
            operation.charge(BudgetDimension::Entries, 1).await?;
            let entry = TreeEntry {
                path: parent.join(&raw.name)?,
                oid: raw.oid,
                mode: raw.mode,
                kind: raw.kind,
                size: None,
            };
            let order_path = ordered_path(&entry);
            if upper
                .as_deref()
                .is_some_and(|upper| order_path.as_ref() >= upper)
            {
                frames.clear();
                break;
            }

            match entry.kind {
                EntryKind::Tree => {
                    if !tree_intersects_prefix(&order_path, &request.prefix) {
                        continue;
                    }
                    if let Some(prefix) =
                        common_prefix(&order_path, &request.prefix, request.delimiter)
                    {
                        emit(
                            TreeListingItem::CommonPrefix(prefix),
                            request.after.as_deref(),
                            &mut items,
                            operation,
                        )
                        .await?;
                        continue;
                    }
                    if request
                        .after
                        .as_deref()
                        .is_some_and(|after| subtree_is_before(&order_path, after))
                    {
                        continue;
                    }
                    frames.push(
                        open_frame(operation, entry.oid, entry.path, entry_depth, lower).await?,
                    );
                }
                EntryKind::Blob if entry.path.as_bytes().starts_with(&request.prefix) => {
                    let item = match common_prefix(
                        entry.path.as_bytes(),
                        &request.prefix,
                        request.delimiter,
                    ) {
                        Some(prefix) => TreeListingItem::CommonPrefix(prefix),
                        None => TreeListingItem::Blob(entry),
                    };
                    emit(item, request.after.as_deref(), &mut items, operation).await?;
                }
                EntryKind::Blob | EntryKind::Symlink | EntryKind::Submodule => {}
            }
        }

        let has_more = items.len() > request.limit;
        items.truncate(request.limit);
        Ok(TreeListingPage { items, has_more })
    }
}

fn next_entry(frames: &mut Vec<TreeFrame>) -> Option<(RawTreeEntry, GitPath, u64)> {
    let frame = frames.last_mut()?;
    let Some(entry) = frame.entries.get(frame.next).cloned() else {
        frames.pop();
        return None;
    };
    frame.next += 1;
    Some((entry, frame.parent.clone(), frame.depth))
}

async fn open_frame(
    operation: &OperationContext,
    oid: ObjectId,
    parent: GitPath,
    depth: u64,
    lower: &[u8],
) -> Result<TreeFrame> {
    operation.charge(BudgetDimension::LogicalObjects, 1).await?;
    let entries = operation.read_raw_tree(oid).await?;
    let (next, comparisons) = seek_entry(&entries, &parent, lower);
    operation
        .charge(BudgetDimension::Entries, comparisons)
        .await?;
    Ok(TreeFrame {
        entries,
        parent,
        next,
        depth,
    })
}

fn seek_entry(entries: &[RawTreeEntry], parent: &GitPath, lower: &[u8]) -> (usize, u64) {
    let parent_bytes = parent.as_bytes();
    let tail = if parent_bytes.is_empty() {
        lower
    } else {
        let Some(tail) = lower
            .strip_prefix(parent_bytes)
            .and_then(|tail| tail.strip_prefix(b"/"))
        else {
            return (0, 0);
        };
        tail
    };
    if tail.is_empty() {
        return (0, 0);
    }
    let (name, target_is_tree) = match tail.iter().position(|byte| *byte == b'/') {
        Some(position) => (&tail[..position], true),
        None => (tail, false),
    };
    let mut start = 0;
    let mut end = entries.len();
    let mut comparisons = 0_u64;
    while start < end {
        let middle = start + (end - start) / 2;
        let entry = &entries[middle];
        comparisons = comparisons.saturating_add(1);
        if tree_name_cmp(
            &entry.name,
            entry.kind == EntryKind::Tree,
            name,
            target_is_tree,
        ) == Ordering::Less
        {
            start = middle + 1;
        } else {
            end = middle;
        }
    }
    (start, comparisons)
}

fn ordered_path(entry: &TreeEntry) -> Bytes {
    if entry.kind != EntryKind::Tree {
        return Bytes::copy_from_slice(entry.path.as_bytes());
    }
    let mut path = Vec::with_capacity(entry.path.as_bytes().len() + 1);
    path.extend_from_slice(entry.path.as_bytes());
    path.push(b'/');
    Bytes::from(path)
}

fn tree_intersects_prefix(subtree: &[u8], prefix: &[u8]) -> bool {
    subtree.starts_with(prefix) || prefix.starts_with(subtree)
}

fn subtree_is_before(subtree: &[u8], after: &[u8]) -> bool {
    subtree <= after && !after.starts_with(subtree)
}

fn common_prefix(path: &[u8], prefix: &[u8], delimiter: Option<u8>) -> Option<Bytes> {
    let delimiter = delimiter?;
    let relative = path.strip_prefix(prefix)?;
    let position = relative.iter().position(|byte| *byte == delimiter)?;
    Some(Bytes::copy_from_slice(&path[..prefix.len() + position + 1]))
}

async fn emit(
    item: TreeListingItem,
    after: Option<&[u8]>,
    items: &mut Vec<TreeListingItem>,
    operation: &OperationContext,
) -> Result<()> {
    if after.is_some_and(|after| item.path() <= after)
        || items.last().is_some_and(|last| last.path() == item.path())
    {
        return Ok(());
    }
    operation
        .charge(BudgetDimension::ResponseBytes, item.path().len() as u64)
        .await?;
    items.push(item);
    Ok(())
}

fn prefix_successor(prefix: &[u8]) -> Option<Bytes> {
    let mut successor = prefix.to_vec();
    let position = successor.iter().rposition(|byte| *byte != u8::MAX)?;
    successor[position] += 1;
    successor.truncate(position + 1);
    Some(Bytes::from(successor))
}
