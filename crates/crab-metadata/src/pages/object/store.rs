use super::*;
use crate::pages::PageWork;
use crab_storage::Store;
use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, StreamExt as _, TryStreamExt as _};
use object_store::path::Path;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CACHE_PAGES: usize = 256;

#[derive(Default)]
struct Cache {
    entries: BTreeMap<[u8; 32], (Kind, Reference, Arc<Node>)>,
    order: VecDeque<[u8; 32]>,
    bytes: usize,
}

pub(crate) struct ObjectPages {
    prefix: Path,
    cache: Mutex<Cache>,
    // One shared set of lanes caps tree-wide reads and verified writes; warm
    // cache hits avoid these locks and do not wait behind cold page requests.
    io: [futures_util::lock::Mutex<()>; 4],
}

impl std::fmt::Debug for ObjectPages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObjectPages")
            .finish_non_exhaustive()
    }
}

impl ObjectPages {
    pub(crate) fn new(prefix: Path) -> Self {
        Self {
            prefix,
            cache: Mutex::default(),
            io: std::array::from_fn(|_| futures_util::lock::Mutex::new(())),
        }
    }

    pub(crate) fn path(&self, hash: &[u8; 32]) -> Path {
        let hex = blake3::Hash::from_bytes(*hash).to_hex().to_string();
        self.prefix.clone().join(&hex[..2]).join(hex)
    }

    fn cached(&self, reference: &Reference, kind: Kind) -> Result<Option<Arc<Node>>> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| corruption("object page cache lock poisoned"))?;
        let Some((prior_kind, prior, node)) = cache.entries.get(&reference.hash) else {
            return Ok(None);
        };
        if *prior_kind != kind || prior != reference {
            return Err(corruption(
                "object page identity has conflicting routing metadata",
            ));
        }
        Ok(Some(node.clone()))
    }

    async fn fetch(&self, store: &Store, reference: &Reference, kind: Kind) -> Result<Arc<Node>> {
        reference.validate(kind)?;
        if let Some(node) = self.cached(reference, kind)? {
            return Ok(node);
        }
        let _read = self.io[usize::from(reference.hash[0]) % self.io.len()]
            .lock()
            .await;
        if let Some(node) = self.cached(reference, kind)? {
            return Ok(node);
        }
        let bytes = store
            .get_with_etag_bounded(&self.path(&reference.hash), u64::from(reference.bytes))
            .await?
            .0;
        let node = Arc::new(decode(&bytes, reference, kind)?);
        self.retain(reference, kind, node.clone())?;
        Ok(node)
    }

    fn retain(&self, reference: &Reference, kind: Kind, node: Arc<Node>) -> Result<()> {
        let weight = resident_bytes(reference, &node);
        if weight > CACHE_BYTES {
            return Ok(());
        }
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| corruption("object page cache lock poisoned"))?;
        if cache.entries.contains_key(&reference.hash) {
            return Ok(());
        }
        while cache.bytes.saturating_add(weight) > CACHE_BYTES || cache.entries.len() >= CACHE_PAGES
        {
            let Some(hash) = cache.order.pop_front() else {
                break;
            };
            if let Some((_, reference, node)) = cache.entries.remove(&hash) {
                cache.bytes = cache
                    .bytes
                    .saturating_sub(resident_bytes(&reference, &node));
            }
        }
        cache.bytes += weight;
        cache.order.push_back(reference.hash);
        cache
            .entries
            .insert(reference.hash, (kind, reference.clone(), node));
        Ok(())
    }

    pub(crate) async fn node(
        &self,
        store: &Store,
        reference: &Reference,
        kind: Kind,
        work: &mut PageWork,
    ) -> Result<Arc<Node>> {
        work.admit_object(reference.hash, u64::from(reference.bytes))?;
        self.fetch(store, reference, kind).await
    }

    pub(crate) async fn lookup(
        &self,
        store: &Store,
        root: Option<&Reference>,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.iter().any(|key| key.len() != kind.key_bytes())
            || (keys.len() as u64)
                .checked_mul((kind.key_bytes() + std::mem::size_of::<Option<Vec<u8>>>()) as u64)
                .is_none_or(|bytes| bytes > work.maximum)
        {
            return Err(corruption(
                "object query keys exceed their shape or byte limit",
            ));
        }
        work.admit_materialized((keys.len() as u64).saturating_mul(
            (kind.key_bytes()
                + std::mem::size_of::<Option<Vec<u8>>>()
                + 3 * std::mem::size_of::<usize>()) as u64,
        ))?;
        let mut output = vec![None; keys.len()];
        let Some(root) = root else {
            return Ok(output);
        };
        root.validate(kind)?;
        let positions = keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| (root.first <= *key && *key <= root.last).then_some(index))
            .collect::<Vec<_>>();
        let mut pending = vec![(root.clone(), positions)];
        while !pending.is_empty() {
            pending.retain(|(_, positions)| !positions.is_empty());
            for (reference, _) in &pending {
                work.admit_object(reference.hash, u64::from(reference.bytes))?;
            }
            let nodes = futures_util::stream::iter(std::mem::take(&mut pending))
                .map(|(reference, positions)| async move {
                    Ok::<_, MetadataError>((self.fetch(store, &reference, kind).await?, positions))
                })
                .buffer_unordered(4)
                .try_collect::<Vec<_>>()
                .await?;
            for (node, positions) in nodes {
                match node.as_ref() {
                    Node::Leaf(rows) => {
                        for index in positions {
                            if let Ok(position) =
                                rows.binary_search_by(|row| row.0.cmp(&keys[index]))
                            {
                                // Repeated keys clone values even when the physical page is shared.
                                work.admit_materialized(2 * (rows[position].1.len() as u64 + 48))?;
                                output[index] = Some(rows[position].1.clone());
                            }
                        }
                    }
                    Node::Branch(children) => {
                        let mut grouped = BTreeMap::<usize, Vec<usize>>::new();
                        for index in positions {
                            let child = children.partition_point(|child| child.last < keys[index]);
                            if let Some(reference) = children.get(child)
                                && reference.first <= keys[index]
                            {
                                grouped.entry(child).or_default().push(index);
                            }
                        }
                        pending.extend(
                            grouped
                                .into_iter()
                                .map(|(index, positions)| (children[index].clone(), positions)),
                        );
                    }
                }
            }
        }
        Ok(output)
    }

    pub(crate) fn walk<'a>(
        &'a self,
        store: &'a Store,
        root: Option<&Reference>,
        kind: Kind,
        work: &'a mut PageWork,
    ) -> BoxStream<'a, Result<(Reference, Arc<Node>)>> {
        let pending = root.cloned().into_iter().collect::<Vec<_>>();
        futures_util::stream::try_unfold((pending, work), move |(mut pending, work)| async move {
            let Some(reference) = pending.pop() else {
                return Ok(None);
            };
            let node = self.node(store, &reference, kind, work).await?;
            match node.as_ref() {
                // Count each logical visit, including repeated nested roots. The
                // second copy admits row clones and their typed catalog values.
                Node::Leaf(_) => work.admit_materialized(
                    (resident_bytes(&reference, &node) as u64).saturating_mul(2),
                )?,
                Node::Branch(children) => pending.extend(children.iter().rev().cloned()),
            }
            Ok(Some(((reference, node), (pending, work))))
        })
        .boxed()
    }

    pub(crate) async fn scan(
        &self,
        store: &Store,
        root: Option<&Reference>,
        kind: Kind,
        work: &mut PageWork,
    ) -> Result<Rows> {
        self.walk(store, root, kind, work)
            .try_fold(Vec::new(), |mut rows, (_, node)| async move {
                if let Node::Leaf(leaf) = node.as_ref() {
                    rows.extend(leaf.iter().cloned());
                }
                Ok(rows)
            })
            .await
    }

    pub(crate) async fn edit(
        &self,
        store: &Store,
        root: Option<&Reference>,
        kind: Kind,
        rows: Rows,
        work: &mut PageWork,
    ) -> Result<Option<Reference>> {
        if rows.is_empty() {
            return Ok(root.cloned());
        }
        validate_rows(kind, &rows)?;
        let incoming_bytes = rows.iter().try_fold(0u64, |bytes, (key, value)| {
            bytes
                .checked_add(key.len() as u64)?
                .checked_add(4)?
                .checked_add(value.len() as u64)
        });
        if incoming_bytes.is_none_or(|bytes| bytes > work.maximum.saturating_sub(work.bytes)) {
            return Err(corruption("object edit input exceeds its byte limit"));
        }
        let work = futures_util::lock::Mutex::new(work);
        let mut level = match root {
            Some(root) => {
                self.edit_node(store, root.clone(), kind, rows, &work)
                    .await?
            }
            None => {
                self.persist(store, kind, leaves(kind, rows)?, &work)
                    .await?
            }
        };
        while level.len() > 1 {
            level = self
                .persist(store, kind, branches(kind, level)?, &work)
                .await?;
        }
        Ok(level.pop())
    }

    fn edit_node<'a>(
        &'a self,
        store: &'a Store,
        reference: Reference,
        kind: Kind,
        rows: Rows,
        work: &'a futures_util::lock::Mutex<&mut PageWork>,
    ) -> BoxFuture<'a, Result<Vec<Reference>>> {
        Box::pin(async move {
            work.lock()
                .await
                .admit_object(reference.hash, u64::from(reference.bytes))?;
            let node = self.fetch(store, &reference, kind).await?;
            match node.as_ref() {
                Node::Leaf(prior) => {
                    let mut merged = prior.iter().cloned().collect::<BTreeMap<_, _>>();
                    merged.extend(rows);
                    let merged = merged.into_iter().collect::<Rows>();
                    if merged == *prior {
                        return Ok(vec![reference]);
                    }
                    self.persist(store, kind, leaves(kind, merged)?, work).await
                }
                Node::Branch(children) => {
                    let mut groups = BTreeMap::<usize, Rows>::new();
                    for row in rows {
                        let index = children.partition_point(|child| child.last < row.0);
                        groups
                            .entry(index.min(children.len() - 1))
                            .or_default()
                            .push(row);
                    }
                    let updated = futures_util::stream::iter(children.iter().cloned().enumerate())
                        .map(|(index, child)| {
                            let rows = groups.remove(&index);
                            async move {
                                match rows {
                                    Some(rows) => {
                                        self.edit_node(store, child, kind, rows, work).await
                                    }
                                    None => Ok(vec![child]),
                                }
                            }
                        })
                        .buffered(4)
                        .try_fold(Vec::new(), |mut updated, children| async move {
                            updated.extend(children);
                            Ok(updated)
                        })
                        .await?;
                    if updated == *children {
                        return Ok(vec![reference]);
                    }
                    self.persist(store, kind, branches(kind, updated)?, work)
                        .await
                }
            }
        })
    }

    async fn persist(
        &self,
        store: &Store,
        kind: Kind,
        pages: Vec<Encoded>,
        work: &futures_util::lock::Mutex<&mut PageWork>,
    ) -> Result<Vec<Reference>> {
        {
            let mut work = work.lock().await;
            for page in &pages {
                work.admit_object(page.reference.hash, u64::from(page.reference.bytes))?;
            }
        }
        futures_util::stream::iter(pages)
            .map(|page| async move {
                let _io = self.io[usize::from(page.reference.hash[0]) % self.io.len()]
                    .lock()
                    .await;
                store
                    .put_if_absent_verified(&self.path(&page.reference.hash), page.bytes.clone())
                    .await?;
                let node = Arc::new(decode(&page.bytes, &page.reference, kind)?);
                self.retain(&page.reference, kind, node)?;
                Ok::<_, MetadataError>(page.reference)
            })
            .buffered(4)
            .try_collect()
            .await
    }
}

fn resident_bytes(reference: &Reference, node: &Node) -> usize {
    let heap = match node {
        Node::Leaf(rows) => rows.iter().fold(
            rows.capacity()
                .saturating_mul(std::mem::size_of::<(Vec<u8>, Vec<u8>)>()),
            |total, (key, value)| {
                total
                    .saturating_add(key.capacity())
                    .saturating_add(value.capacity())
            },
        ),
        Node::Branch(children) => children.iter().fold(
            children
                .capacity()
                .saturating_mul(std::mem::size_of::<Reference>()),
            |total, child| {
                total
                    .saturating_add(child.first.capacity())
                    .saturating_add(child.last.capacity())
            },
        ),
    };
    std::mem::size_of::<Node>()
        .saturating_add(std::mem::size_of::<([u8; 32], (Kind, Reference, Arc<Node>))>())
        .saturating_add(std::mem::size_of::<[u8; 32]>())
        .saturating_add(3 * std::mem::size_of::<usize>())
        .saturating_add(reference.first.capacity())
        .saturating_add(reference.last.capacity())
        .saturating_add(heap)
}
