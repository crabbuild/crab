//! Lazy lookups bound to an authenticated descriptor; full scans remain maintenance operations.
use crab_storage::{Store, StoreLayout};
use futures_util::{StreamExt, TryStreamExt};
use std::sync::{Mutex, MutexGuard};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use super::paged::{self, Kind, Page, PathStatePageRef};
use super::{
    PathStateDescriptor, PathStateNodeRef, PathStateRecord, PathStateSummary, Result, codec,
    corrupt, corruption, validate_path,
};
use crate::split_commit_graph::{CommitGraphLayerRef, SplitCommitGraph};

const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CACHE_PAGES: usize = 256;

/// Verified path attribution with bounded, shared immutable-page caching.
#[derive(Debug)]
pub struct PathStateReader {
    descriptor: PathStateDescriptor,
    graph_layers: Vec<CommitGraphLayerRef>,
    paths: Vec<object_store::path::Path>,
    descriptor_bytes: u64,
    max_bytes: u64,
    cache: Mutex<Cache>,
}

#[derive(Debug, Default)]
struct Cache {
    pages: BTreeMap<(usize, u64), (Kind, PathStatePageRef, Arc<Page>)>,
    order: VecDeque<(usize, u64)>,
    bytes: usize,
}

struct ChildBatch {
    root: PathStatePageRef,
    requests: Vec<(usize, u32)>,
}

struct Work {
    maximum: u64,
    bytes: u64,
    seen: BTreeSet<(usize, u64)>,
}

impl PathStateReader {
    /// Load only a descriptor and bind its positional roots to the supplied graph.
    pub async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        hash: &str,
        graph: &SplitCommitGraph,
        max_bytes: u64,
    ) -> Result<Self> {
        let (descriptor, descriptor_bytes) =
            super::storage::load_path_state_descriptor_with_size(store, layout, hash, max_bytes)
                .await?;
        if !matches_graph(&descriptor, graph)
            || descriptor.commit_ordinal_digest != graph.ordinal_digest()
        {
            return corrupt("path-state descriptor does not match its commit graph");
        }
        let paths = descriptor
            .layers
            .iter()
            .map(|layer| layout.repo_path(&layer.path))
            .collect();
        Ok(Self {
            descriptor,
            graph_layers: graph.descriptor.layers.clone(),
            paths,
            descriptor_bytes,
            max_bytes,
            cache: Mutex::new(Cache::default()),
        })
    }

    /// Resolve raw paths using the captured graph and an operation-admitted store for the same origin.
    pub async fn latest(
        &self,
        store: &Store,
        graph: &SplitCommitGraph,
        start: &[u8; 20],
        paths: &[&[u8]],
    ) -> Result<Vec<PathStateSummary>> {
        // Scalars can match two rebuilt graphs with different ordinals. The
        // immutable layer identities pin the ordering verified during load.
        if !matches_graph(&self.descriptor, graph) || self.graph_layers != graph.descriptor.layers {
            return corrupt("path-state query graph identity changed");
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        for path in paths {
            validate_path(path, false)?;
        }
        let start_ordinal = graph
            .ordinal(start)
            .ok_or_else(|| corruption("start commit is absent from graph"))?;
        let mut work = Work {
            maximum: self.max_bytes,
            bytes: self.descriptor_bytes,
            seen: BTreeSet::new(),
        };
        let start = self
            .records(store, graph, &[start_ordinal], &mut work)
            .await?
            .remove(0);
        let mut nodes = vec![start.root; paths.len()];
        let mut remaining = paths
            .iter()
            .map(|path| path.split(|byte| *byte == b'/'))
            .collect::<Vec<_>>();
        let mut pending = (0..paths.len()).collect::<Vec<_>>();
        while !pending.is_empty() {
            let states = self.nodes(store, &nodes, &pending, &mut work).await?;
            let mut components = BTreeMap::<usize, Vec<(usize, &[u8], PathStatePageRef)>>::new();
            for (&position, state) in pending.iter().zip(states) {
                let Some(component) = remaining[position].next() else {
                    continue;
                };
                let children = state
                    .1
                    .ok_or_else(|| corruption("visible path is absent from its trie"))?;
                components
                    .entry(nodes[position].layer as usize)
                    .or_default()
                    .push((position, component, children));
            }
            let mut children = BTreeMap::<(usize, u64), ChildBatch>::new();
            for (layer, requests) in components {
                let names = self.descriptor.layers[layer]
                    .pages
                    .names
                    .as_ref()
                    .ok_or_else(|| corruption("children have no component directory"))?;
                let keys = requests
                    .iter()
                    .map(|(_, name, _)| blake3::hash(name).as_bytes().to_vec())
                    .collect::<Vec<_>>();
                let values = self
                    .lookup(store, layer, names, Kind::Names, &keys, &mut work)
                    .await?;
                for ((position, component, root), (key, value)) in
                    requests.into_iter().zip(keys.into_iter().zip(values))
                {
                    let value = value.ok_or_else(|| corruption("visible component is absent"))?;
                    let (ordinal, name) =
                        codec::decode_name(&key, &value, self.paths[layer].as_ref())?;
                    if name != component || ordinal >= names.count {
                        return corrupt("component identity or ordinal mismatch");
                    }
                    let batch =
                        children
                            .entry((layer, root.offset))
                            .or_insert_with(|| ChildBatch {
                                root: root.clone(),
                                requests: Vec::new(),
                            });
                    if batch.root != root {
                        return corrupt("shared child roots have conflicting routing metadata");
                    }
                    batch.requests.push((position, ordinal));
                }
            }
            pending.clear();
            for ((layer, _), batch) in children {
                let keys = batch
                    .requests
                    .iter()
                    .map(|(_, ordinal)| ordinal.to_be_bytes().to_vec())
                    .collect::<Vec<_>>();
                let values = self
                    .lookup(store, layer, &batch.root, Kind::Children, &keys, &mut work)
                    .await?;
                for ((position, _), value) in batch.requests.into_iter().zip(values) {
                    let value = value.ok_or_else(|| corruption("visible child is absent"))?;
                    let child = codec::decode_child(&value, self.paths[layer].as_ref())?;
                    self.validate_node(child, Some(nodes[position]))?;
                    nodes[position] = child;
                    pending.push(position);
                }
            }
        }
        let positions = (0..paths.len()).collect::<Vec<_>>();
        let states = self.nodes(store, &nodes, &positions, &mut work).await?;
        let ordinals = states
            .into_iter()
            .map(|(value, _)| {
                value
                    .filter(|ordinal| *ordinal <= start_ordinal)
                    .ok_or_else(|| {
                        corruption("path attribution is absent or points to a future commit")
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(self
            .records(store, graph, &ordinals, &mut work)
            .await?
            .into_iter()
            .map(|record| PathStateSummary {
                oid: record.oid,
                author: record.author,
                author_seconds: record.author_seconds,
                message: record.message,
            })
            .collect())
    }

    async fn records(
        &self,
        store: &Store,
        graph: &SplitCommitGraph,
        ordinals: &[u32],
        work: &mut Work,
    ) -> Result<Vec<PathStateRecord>> {
        let mut grouped = BTreeMap::<usize, Vec<(usize, u32)>>::new();
        for (position, &ordinal) in ordinals.iter().enumerate() {
            let layer = self
                .descriptor
                .layers
                .partition_point(|layer| layer.base_ordinal <= ordinal)
                .checked_sub(1)
                .ok_or_else(|| corruption("record layer is absent"))?;
            let reference = &self.descriptor.layers[layer];
            if ordinal - reference.base_ordinal >= reference.commit_count {
                return corrupt("record ordinal is out of bounds");
            }
            grouped.entry(layer).or_default().push((position, ordinal));
        }
        let mut output = vec![None; ordinals.len()];
        for (layer, requests) in grouped {
            let reference = &self.descriptor.layers[layer];
            let keys = requests
                .iter()
                .map(|(_, ordinal)| (ordinal - reference.base_ordinal).to_be_bytes().to_vec())
                .collect::<Vec<_>>();
            let values = self
                .lookup(
                    store,
                    layer,
                    &reference.pages.records,
                    Kind::Records,
                    &keys,
                    work,
                )
                .await?;
            for ((position, ordinal), value) in requests.into_iter().zip(values) {
                let value = value.ok_or_else(|| corruption("positional record is missing"))?;
                let record = codec::decode_record(&value, self.paths[layer].as_ref())?;
                let commit = graph
                    .record(ordinal)
                    .ok_or_else(|| corruption("commit graph record is missing"))?;
                if record.oid != commit.oid
                    || record.first_parent != commit.parents.first().copied()
                {
                    return corrupt("path-state record does not match its commit ordinal");
                }
                self.validate_node(record.root, None)?;
                if record.root.layer as usize > layer {
                    return corrupt("record root points to a future layer");
                }
                output[position] = Some(record);
            }
        }
        output
            .into_iter()
            .map(|record| record.ok_or_else(|| corruption("record result is incomplete")))
            .collect()
    }

    async fn nodes(
        &self,
        store: &Store,
        nodes: &[PathStateNodeRef],
        positions: &[usize],
        work: &mut Work,
    ) -> Result<Vec<(Option<u32>, Option<PathStatePageRef>)>> {
        let mut grouped = BTreeMap::<usize, Vec<(usize, PathStateNodeRef)>>::new();
        for (output, &position) in positions.iter().enumerate() {
            let node = nodes[position];
            self.validate_node(node, None)?;
            grouped
                .entry(node.layer as usize)
                .or_default()
                .push((output, node));
        }
        let mut output = vec![None; positions.len()];
        for (layer, requests) in grouped {
            let reference = &self.descriptor.layers[layer];
            let keys = requests
                .iter()
                .map(|(_, node)| node.index.to_be_bytes().to_vec())
                .collect::<Vec<_>>();
            let values = self
                .lookup(
                    store,
                    layer,
                    &reference.pages.nodes,
                    Kind::Nodes,
                    &keys,
                    work,
                )
                .await?;
            for ((position, _), value) in requests.into_iter().zip(values) {
                let value = value.ok_or_else(|| corruption("positional node is missing"))?;
                let node = codec::decode_node(&value, reference.bytes, self.paths[layer].as_ref())?;
                if node
                    .0
                    .is_some_and(|ordinal| ordinal >= self.descriptor.commit_count)
                {
                    return corrupt("node attribution is out of bounds");
                }
                output[position] = Some(node);
            }
        }
        output
            .into_iter()
            .map(|node| node.ok_or_else(|| corruption("node result is incomplete")))
            .collect()
    }

    fn validate_node(
        &self,
        node: PathStateNodeRef,
        parent: Option<PathStateNodeRef>,
    ) -> Result<()> {
        let layer = self
            .descriptor
            .layers
            .get(node.layer as usize)
            .ok_or_else(|| corruption("node layer is absent"))?;
        if node.index >= layer.node_count
            || parent.is_some_and(|parent| {
                node.layer > parent.layer
                    || (node.layer == parent.layer && node.index >= parent.index)
            })
        {
            return corrupt("node reference is out of bounds or cyclic");
        }
        Ok(())
    }

    async fn lookup(
        &self,
        store: &Store,
        layer: usize,
        root: &PathStatePageRef,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut Work,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.iter().any(|key| key.len() != kind.key_bytes()) {
            return corrupt("invalid lookup key");
        }
        let mut pending = vec![(root.clone(), (0..keys.len()).collect::<Vec<_>>())];
        let mut results = vec![None; keys.len()];
        while !pending.is_empty() {
            let wave = std::mem::take(&mut pending);
            for (reference, _) in &wave {
                self.charge(layer, reference, kind, work)?;
            }
            let pages = futures_util::stream::iter(wave.into_iter().map(
                |(reference, positions)| async move {
                    let page = self.page(store, layer, &reference, kind).await?;
                    Ok::<_, crate::error::MetadataError>((page, positions))
                },
            ))
            .buffer_unordered(16)
            .try_collect::<Vec<_>>()
            .await?;
            for (page, positions) in pages {
                match page.as_ref() {
                    Page::Leaf(rows) => {
                        for position in positions {
                            if let Ok(index) =
                                rows.binary_search_by(|row| row.0.cmp(&keys[position]))
                            {
                                results[position] = Some(rows[index].1.clone());
                            }
                        }
                    }
                    Page::Branch(children) => {
                        let mut batches = BTreeMap::<usize, Vec<usize>>::new();
                        for position in positions {
                            let key = &keys[position];
                            let index = children.partition_point(|child| child.last < *key);
                            if children.get(index).is_some_and(|child| child.first <= *key) {
                                batches.entry(index).or_default().push(position);
                            }
                        }
                        pending.extend(
                            batches
                                .into_iter()
                                .map(|(index, positions)| (children[index].clone(), positions)),
                        );
                    }
                }
            }
        }
        Ok(results)
    }

    fn charge(
        &self,
        layer: usize,
        reference: &PathStatePageRef,
        kind: Kind,
        work: &mut Work,
    ) -> Result<()> {
        let source_bytes = self.descriptor.layers[layer].bytes;
        reference.validate(kind, source_bytes)?;
        let key = (layer, reference.offset);
        if work.seen.insert(key) {
            work.bytes = work
                .bytes
                .checked_add(u64::from(reference.bytes))
                .ok_or_else(|| corruption("query byte count overflows"))?;
            if work.bytes > work.maximum {
                return corrupt("path-state query exceeds its byte limit");
            }
        }
        Ok(())
    }

    // Guards cover local cache work only; origin I/O runs after the guard drops.
    fn cache(&self) -> Result<MutexGuard<'_, Cache>> {
        self.cache.lock().map_err(|_| {
            crate::error::MetadataError::Internal("path-state page cache is poisoned".to_owned())
        })
    }

    async fn page(
        &self,
        store: &Store,
        layer: usize,
        reference: &PathStatePageRef,
        kind: Kind,
    ) -> Result<Arc<Page>> {
        let source_bytes = self.descriptor.layers[layer].bytes;
        let key = (layer, reference.offset);
        if let Some((previous_kind, previous, page)) = self.cache()?.pages.get(&key) {
            if *previous_kind != kind || previous != reference {
                return corrupt("cached page routing metadata changed");
            }
            return Ok(page.clone());
        }
        let bytes = store
            .range_get(
                &self.paths[layer],
                reference.offset..reference.offset + u64::from(reference.bytes),
            )
            .await?;
        let page = Arc::new(paged::decode(
            &bytes,
            reference,
            kind,
            source_bytes,
            self.paths[layer].as_ref(),
        )?);
        let weight = page_weight(reference, &page);
        if weight <= CACHE_BYTES {
            let mut cache = self.cache()?;
            if let Some((previous_kind, previous, existing)) = cache.pages.get(&key) {
                if *previous_kind != kind || previous != reference {
                    return corrupt("concurrent page routing metadata changed");
                }
                return Ok(existing.clone());
            }
            while cache.bytes.saturating_add(weight) > CACHE_BYTES
                || cache.pages.len() >= CACHE_PAGES
            {
                let Some(oldest) = cache.order.pop_front() else {
                    break;
                };
                if let Some((_, reference, old)) = cache.pages.remove(&oldest) {
                    cache.bytes = cache.bytes.saturating_sub(page_weight(&reference, &old));
                }
            }
            cache.bytes += weight;
            cache.order.push_back(key);
            cache
                .pages
                .insert(key, (kind, reference.clone(), page.clone()));
        }
        Ok(page)
    }
}

fn page_weight(reference: &PathStatePageRef, page: &Page) -> usize {
    page.resident_bytes()
        .saturating_add(std::mem::size_of::<Page>())
        .saturating_add(std::mem::size_of::<PathStatePageRef>())
        .saturating_add(reference.first.capacity())
        .saturating_add(reference.last.capacity())
        .saturating_add(128)
}

fn matches_graph(descriptor: &PathStateDescriptor, graph: &SplitCommitGraph) -> bool {
    descriptor.generation == graph.descriptor.generation
        && descriptor.pack_index_hash == graph.descriptor.pack_index_hash
        && descriptor.git_validation_digest == graph.descriptor.git_validation_digest
        && descriptor.commit_count == graph.descriptor.commit_count
}

#[cfg(test)]
#[path = "reader_tests.rs"]
mod tests;
