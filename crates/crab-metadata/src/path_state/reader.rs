//! Lazy lookups bound to an authenticated descriptor; full scans remain maintenance operations.
use crab_storage::{Store, StoreLayout};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::{
    PathStateDescriptor, PathStateNodeRef, PathStateRecord, PathStateSummary, Result, codec,
    corrupt, corruption, validate_path,
};
use crate::pages::{self as paged, Kind, Page, PageRef, PageWork};
use crate::split_commit_graph::{CommitGraphLayerRef, CommitGraphView, SplitCommitGraph};

#[path = "scan.rs"]
mod scan;
pub use scan::{PathStateItem, PathStateScan};

/// Verified path attribution with bounded, shared immutable-page caching.
#[derive(Debug)]
pub struct PathStateReader {
    descriptor: PathStateDescriptor,
    graph_layers: Vec<CommitGraphLayerRef>,
    paths: Vec<object_store::path::Path>,
    descriptor_bytes: u64,
    max_bytes: u64,
    cache: paged::PageCache,
}

struct ChildBatch {
    root: PageRef,
    requests: Vec<(usize, u32)>,
}

impl PathStateReader {
    /// Load only a descriptor and bind its positional roots to the supplied graph.
    pub async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        hash: &str,
        graph: CommitGraphView<'_>,
        max_bytes: u64,
    ) -> Result<Self> {
        let (descriptor, descriptor_bytes) =
            super::storage::load_path_state_descriptor_with_size(store, layout, hash, max_bytes)
                .await?;
        if !matches_graph(&descriptor, graph)
            || descriptor.commit_ordinal_digest != graph.ordinal_digest()?
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
            graph_layers: graph.descriptor().layers.clone(),
            paths,
            descriptor_bytes,
            max_bytes,
            cache: paged::PageCache::default(),
        })
    }

    /// Resolve raw paths using the captured graph and an operation-admitted store for the same origin.
    pub async fn latest(
        &self,
        store: &Store,
        graph: CommitGraphView<'_>,
        start: &[u8; 20],
        paths: &[&[u8]],
    ) -> Result<Vec<PathStateSummary>> {
        self.summaries(store, graph, start, paths, true)
            .await?
            .into_iter()
            .map(|summary| summary.ok_or_else(|| corruption("visible path has no attribution")))
            .collect()
    }

    /// Locate last path changes, including deletion and covering type resets.
    pub async fn last_change(
        &self,
        store: &Store,
        graph: CommitGraphView<'_>,
        start: &[u8; 20],
        paths: &[&[u8]],
    ) -> Result<Vec<Option<[u8; 20]>>> {
        Ok(self
            .summaries(store, graph, start, paths, false)
            .await?
            .into_iter()
            .map(|summary| summary.map(|summary| summary.oid))
            .collect())
    }

    async fn summaries(
        &self,
        store: &Store,
        graph: CommitGraphView<'_>,
        start: &[u8; 20],
        paths: &[&[u8]],
        visible: bool,
    ) -> Result<Vec<Option<PathStateSummary>>> {
        // Scalars can match two rebuilt graphs with different ordinals. The
        // immutable layer identities pin the ordering verified during load.
        if !matches_graph(&self.descriptor, graph) || self.graph_layers != graph.descriptor().layers
        {
            return corrupt("path-state query graph identity changed");
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        for path in paths {
            validate_path(path, false)?;
        }
        let start_ordinal = graph
            .ordinals(store, &[*start])
            .await?
            .remove(0)
            .ok_or_else(|| corruption("start commit is absent from graph"))?;
        let mut work = PageWork {
            maximum: self.max_bytes,
            bytes: self.descriptor_bytes,
            seen: BTreeSet::new(),
        };
        let start = self
            .records(store, graph, &[start_ordinal], &mut work)
            .await?
            .remove(0);
        let mut nodes = vec![start.root; paths.len()];
        let mut resets = vec![None; paths.len()];
        let mut found = vec![true; paths.len()];
        let mut remaining = paths
            .iter()
            .map(|path| path.split(|byte| *byte == b'/'))
            .collect::<Vec<_>>();
        let mut pending = (0..paths.len()).collect::<Vec<_>>();
        while !pending.is_empty() {
            let states = self.nodes(store, &nodes, &pending, &mut work).await?;
            let mut components = BTreeMap::<usize, Vec<(usize, &[u8], PageRef)>>::new();
            for (&position, state) in pending.iter().zip(states) {
                resets[position] = resets[position].max(state.1);
                let Some(component) = remaining[position].next() else {
                    continue;
                };
                let Some(children) = state.2 else {
                    found[position] = false;
                    continue;
                };
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
                    .cache
                    .lookup(
                        store,
                        &self.paths[layer],
                        layer,
                        0..self.descriptor.layers[layer].bytes,
                        names,
                        Kind::Names,
                        &keys,
                        &mut work,
                    )
                    .await?;
                for ((position, component, root), (key, value)) in
                    requests.into_iter().zip(keys.into_iter().zip(values))
                {
                    let Some(value) = value else {
                        found[position] = false;
                        continue;
                    };
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
                    .cache
                    .lookup(
                        store,
                        &self.paths[layer],
                        layer,
                        0..self.descriptor.layers[layer].bytes,
                        &batch.root,
                        Kind::Children,
                        &keys,
                        &mut work,
                    )
                    .await?;
                for ((position, _), value) in batch.requests.into_iter().zip(values) {
                    let Some(value) = value else {
                        found[position] = false;
                        continue;
                    };
                    let child = codec::decode_child(&value, self.paths[layer].as_ref())?;
                    self.validate_node(child, Some(nodes[position]))?;
                    nodes[position] = child;
                    pending.push(position);
                }
            }
        }
        let positions = (0..paths.len())
            .filter(|position| found[*position])
            .collect::<Vec<_>>();
        let states = self.nodes(store, &nodes, &positions, &mut work).await?;
        let mut ordinals = resets.clone();
        for (position, (value, reset, _)) in positions.into_iter().zip(states) {
            let reset = resets[position].max(reset);
            ordinals[position] = if visible {
                value.filter(|value| reset.is_none_or(|reset| *value >= reset))
            } else {
                value.max(reset)
            };
        }
        if visible {
            for (ordinal, found) in ordinals.iter_mut().zip(found) {
                if !found {
                    *ordinal = None;
                }
                if ordinal.is_none() {
                    return corrupt("visible path is absent from its trie");
                }
            }
        }
        if ordinals
            .iter()
            .flatten()
            .any(|ordinal| *ordinal > start_ordinal)
        {
            return corrupt("path attribution points to a future commit");
        }
        let requested = ordinals.iter().flatten().copied().collect::<Vec<_>>();
        let mut records = self
            .records(store, graph, &requested, &mut work)
            .await?
            .into_iter();
        ordinals
            .into_iter()
            .map(|ordinal| {
                ordinal
                    .map(|_| {
                        let record = records
                            .next()
                            .ok_or_else(|| corruption("path summary is missing"))?;
                        Ok(PathStateSummary {
                            oid: record.oid,
                            author: record.author,
                            author_seconds: record.author_seconds,
                            message: record.message,
                        })
                    })
                    .transpose()
            })
            .collect()
    }

    async fn records(
        &self,
        store: &Store,
        graph: CommitGraphView<'_>,
        ordinals: &[u32],
        work: &mut PageWork,
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
        let commits = graph.records(store, ordinals).await?;
        for (layer, requests) in grouped {
            let reference = &self.descriptor.layers[layer];
            let keys = requests
                .iter()
                .map(|(_, ordinal)| (ordinal - reference.base_ordinal).to_be_bytes().to_vec())
                .collect::<Vec<_>>();
            let values = self
                .cache
                .lookup(
                    store,
                    &self.paths[layer],
                    layer,
                    0..self.descriptor.layers[layer].bytes,
                    &reference.pages.records,
                    Kind::Records,
                    &keys,
                    work,
                )
                .await?;
            for ((position, _), value) in requests.into_iter().zip(values) {
                let value = value.ok_or_else(|| corruption("positional record is missing"))?;
                let record = codec::decode_record(&value, self.paths[layer].as_ref())?;
                let commit = commits[position]
                    .as_ref()
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
        work: &mut PageWork,
    ) -> Result<Vec<(Option<u32>, Option<u32>, Option<PageRef>)>> {
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
                .cache
                .lookup(
                    store,
                    &self.paths[layer],
                    layer,
                    0..self.descriptor.layers[layer].bytes,
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
                    .into_iter()
                    .chain(node.1)
                    .any(|ordinal| ordinal >= self.descriptor.commit_count)
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

    async fn page(
        &self,
        store: &Store,
        layer: usize,
        reference: &PageRef,
        kind: Kind,
    ) -> Result<Arc<Page>> {
        self.cache
            .fetch(
                store,
                &self.paths[layer],
                layer,
                0..self.descriptor.layers[layer].bytes,
                reference,
                kind,
            )
            .await
    }
}

fn matches_graph(descriptor: &PathStateDescriptor, graph: CommitGraphView<'_>) -> bool {
    descriptor.generation == graph.descriptor().generation
        && descriptor.pack_index_hash == graph.descriptor().pack_index_hash
        && descriptor.git_validation_digest == graph.descriptor().git_validation_digest
        && descriptor.commit_count == graph.descriptor().commit_count
}

#[cfg(test)]
#[path = "reader_tests.rs"]
mod tests;
