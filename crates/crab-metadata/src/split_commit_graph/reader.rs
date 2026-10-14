use super::paged::{CommitGraphLayerPages, record};
use super::*;
use crate::PageRef;
use crate::pages::{self, Kind, Page};
use futures_util::{StreamExt, TryStreamExt};
use std::collections::BTreeSet;

/// Immutable graph point lookups; record presence does not prove ref visibility.
#[derive(Debug)]
pub struct CommitGraphReader {
    descriptor: CommitGraphDescriptor,
    paths: Vec<object_store::path::Path>,
    descriptor_bytes: u64,
    max_bytes: u64,
    cache: pages::PageCache,
}

struct Work {
    bytes: u64,
    seen: BTreeSet<(usize, u64)>,
}

/// Read-only positional access for maintenance graphs and immutable paged readers.
#[derive(Clone, Copy)]
pub enum CommitGraphView<'a> {
    Materialized(&'a SplitCommitGraph),
    Paged(&'a CommitGraphReader),
}

impl CommitGraphView<'_> {
    /// Return the descriptor defining this view's positional identity.
    pub fn descriptor(&self) -> &CommitGraphDescriptor {
        match self {
            Self::Materialized(graph) => &graph.descriptor,
            Self::Paged(graph) => graph.descriptor(),
        }
    }

    pub(crate) fn ordinal_digest(&self) -> Result<std::borrow::Cow<'_, str>> {
        match self {
            Self::Materialized(graph) => Ok(graph.ordinal_digest().into()),
            Self::Paged(graph) => graph
                .descriptor
                .commit_ordinal_digest
                .as_deref()
                .map(std::borrow::Cow::Borrowed)
                .ok_or_else(|| {
                    crate::validation::corrupt_object("graph", "positional identity is absent")
                }),
        }
    }

    /// Resolve OIDs while preserving requested positions and duplicates.
    pub async fn ordinals(&self, store: &Store, oids: &[[u8; 20]]) -> Result<Vec<Option<u32>>> {
        match self {
            Self::Materialized(graph) => Ok(oids.iter().map(|oid| graph.ordinal(oid)).collect()),
            Self::Paged(graph) => graph.ordinals(store, oids).await,
        }
    }

    /// Resolve the requested records, validating the consumed paged parent edges.
    pub async fn records(
        &self,
        store: &Store,
        ordinals: &[u32],
    ) -> Result<Vec<Option<CommitGraphRecord>>> {
        match self {
            Self::Materialized(graph) => Ok(ordinals
                .iter()
                .map(|ordinal| graph.record(*ordinal).cloned())
                .collect()),
            Self::Paged(graph) => graph.records(store, ordinals).await,
        }
    }
}

impl CommitGraphReader {
    /// Test membership in a captured commit's verified ancestor set, including itself.
    ///
    /// The caller authorizes the root against captured refs and admits request/output counts.
    pub async fn ancestry_membership(
        &self,
        store: &Store,
        root: u32,
        candidates: &[u32],
    ) -> Result<Vec<bool>> {
        if root >= self.descriptor.commit_count {
            return corrupt("ancestry root ordinal is absent");
        }
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let layer = self
            .descriptor
            .layers
            .partition_point(|layer| layer.base_ordinal <= root)
            .checked_sub(1)
            .ok_or_else(|| crate::validation::corrupt_object("ancestry", "root has no layer"))?;
        let mut work = self.work();
        let values = self
            .lookup(
                store,
                layer,
                &self.roots(&self.descriptor.layers[layer])?.ancestry_roots,
                Kind::CommitAncestryRoots,
                &[root.to_be_bytes().to_vec()],
                &mut work,
            )
            .await?;
        let reference = ancestry::Reference::decode(
            values[0].as_deref().ok_or_else(|| {
                crate::validation::corrupt_object("ancestry", "required root is absent")
            })?,
            "ancestry root",
        )?;
        let mut requested = candidates.to_vec();
        requested.push(root);
        let mut output = vec![false; requested.len()];
        let mut pending = vec![(reference, (0..requested.len()).collect::<Vec<_>>())];
        while !pending.is_empty() {
            let references = pending
                .iter()
                .map(|(reference, _)| *reference)
                .collect::<Vec<_>>();
            let nodes = self.ancestry_nodes(store, &references, &mut work).await?;
            let mut next = BTreeMap::<[u8; 32], (ancestry::Reference, Vec<usize>)>::new();
            for ((_, positions), node) in std::mem::take(&mut pending).into_iter().zip(nodes) {
                if node.children.is_none() && node.reference.end() > u64::from(root) + 1 {
                    return corrupt("ancestry set contains a future ordinal");
                }
                for position in positions {
                    let candidate = requested[position];
                    if candidate > root || !node.reference.contains(candidate) {
                        continue;
                    }
                    let Some(children) = node.children else {
                        output[position] = true;
                        continue;
                    };
                    if let Some(child) =
                        children.into_iter().find(|child| child.contains(candidate))
                    {
                        let batch = next.entry(child.hash).or_insert((child, Vec::new()));
                        if batch.0 != child {
                            return corrupt("ancestry node has conflicting routing references");
                        }
                        batch.1.push(position);
                    }
                }
            }
            pending = next.into_values().collect();
        }
        if output.pop() != Some(true) {
            return corrupt("ancestry root does not contain itself");
        }
        Ok(output)
    }

    async fn ancestry_nodes(
        &self,
        store: &Store,
        references: &[ancestry::Reference],
        work: &mut Work,
    ) -> Result<Vec<ancestry::Node>> {
        let keys = references
            .iter()
            .map(|reference| reference.hash.to_vec())
            .collect::<Vec<_>>();
        let mut output = vec![None; references.len()];
        for (layer, reference) in self.descriptor.layers.iter().enumerate() {
            let values = self
                .lookup(
                    store,
                    layer,
                    &self.roots(reference)?.ancestry_nodes,
                    Kind::CommitAncestryNodes,
                    &keys,
                    work,
                )
                .await?;
            for (index, value) in values.into_iter().enumerate() {
                let Some(value) = value else { continue };
                if output[index].is_some() {
                    return corrupt("ancestry node occurs in multiple layers");
                }
                output[index] = Some(ancestry::Node::decode(
                    &value,
                    references[index],
                    "ancestry node",
                )?);
            }
        }
        output
            .into_iter()
            .map(|node| {
                node.ok_or_else(|| {
                    crate::validation::corrupt_object("ancestry", "required node is absent")
                })
            })
            .collect()
    }

    /// Load a bounded paged descriptor without fetching graph layers.
    pub async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        hash: &str,
        max_bytes: u64,
    ) -> Result<Self> {
        let (descriptor, descriptor_bytes) =
            read_split_commit_graph_descriptor(store, layout, hash, max_bytes.min(128 * 1024))
                .await?;
        if descriptor.version != PAGED_VERSION {
            return corrupt("graph is not the capsule paged format");
        }
        let paths = descriptor
            .layers
            .iter()
            .map(|layer| layout.repo_path(&layer.path))
            .collect();
        Ok(Self {
            descriptor,
            paths,
            descriptor_bytes,
            max_bytes,
            cache: pages::PageCache::default(),
        })
    }

    /// Return the captured immutable descriptor, including positional identity.
    #[must_use]
    pub fn descriptor(&self) -> &CommitGraphDescriptor {
        &self.descriptor
    }

    /// Resolve requested OIDs, retaining duplicate input occurrences.
    ///
    /// The caller admits request/output counts and supplies an operation-budgeted store.
    pub async fn ordinals(&self, store: &Store, oids: &[[u8; 20]]) -> Result<Vec<Option<u32>>> {
        let mut work = self.work();
        let mut output = vec![None; oids.len()];
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        for (layer, reference) in self.descriptor.layers.iter().enumerate() {
            let roots = self.roots(reference)?;
            let values = self
                .lookup(
                    store,
                    layer,
                    &roots.oids,
                    Kind::CommitOids,
                    &keys,
                    &mut work,
                )
                .await?;
            for (index, value) in values.into_iter().enumerate() {
                let Some(value) = value else { continue };
                let ordinal = u32::from_le_bytes(value.as_slice().try_into().map_err(|_| {
                    crate::validation::corrupt_object("graph OID", "invalid ordinal value")
                })?);
                if ordinal < reference.base_ordinal
                    || ordinal - reference.base_ordinal >= reference.commit_count
                    || output[index].is_some()
                {
                    return corrupt("graph OID mapping escapes its layer or repeats");
                }
                output[index] = Some(ordinal);
            }
        }
        let ordinals = output.iter().flatten().copied().collect::<Vec<_>>();
        let records = self.raw_records(store, &ordinals, &mut work).await?;
        let mut records = records.into_iter();
        for (oid, ordinal) in oids.iter().zip(&output) {
            if ordinal.is_some()
                && records
                    .next()
                    .flatten()
                    .is_none_or(|record| record.oid != *oid)
            {
                return corrupt("graph OID mapping disagrees with its record");
            }
        }
        Ok(output)
    }

    /// Resolve records and verify ordered parent edges and corrected generations.
    ///
    /// The caller admits request/output counts and supplies an operation-budgeted store.
    pub async fn records(
        &self,
        store: &Store,
        ordinals: &[u32],
    ) -> Result<Vec<Option<CommitGraphRecord>>> {
        let mut work = self.work();
        let output = self.raw_records(store, ordinals, &mut work).await?;
        let parent_ids = output
            .iter()
            .flatten()
            .flat_map(|record| record.parents.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let parent_records = self.raw_records(store, &parent_ids, &mut work).await?;
        let parents = parent_ids
            .into_iter()
            .zip(parent_records)
            .collect::<BTreeMap<_, _>>();
        for record in output.iter().flatten() {
            let mut generation = non_negative_time(record.commit_time);
            for parent in &record.parents {
                let parent = parents
                    .get(parent)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| {
                        crate::validation::corrupt_object(
                            "graph parents",
                            "required parent is absent",
                        )
                    })?;
                generation = generation.max(parent.corrected_generation.saturating_add(1));
            }
            if generation != record.corrected_generation {
                return corrupt("graph corrected generation mismatch");
            }
        }
        Ok(output)
    }

    fn work(&self) -> Work {
        Work {
            bytes: self.descriptor_bytes,
            seen: BTreeSet::new(),
        }
    }

    fn roots<'a>(&self, layer: &'a CommitGraphLayerRef) -> Result<&'a CommitGraphLayerPages> {
        layer
            .pages
            .as_ref()
            .ok_or_else(|| crate::validation::corrupt_object("graph", "paged roots are absent"))
    }

    async fn raw_records(
        &self,
        store: &Store,
        ordinals: &[u32],
        work: &mut Work,
    ) -> Result<Vec<Option<CommitGraphRecord>>> {
        let mut output = vec![None; ordinals.len()];
        let mut requests = BTreeMap::<usize, Vec<(usize, Vec<u8>)>>::new();
        for (index, ordinal) in ordinals.iter().copied().enumerate() {
            if ordinal >= self.descriptor.commit_count {
                continue;
            }
            let layer = self
                .descriptor
                .layers
                .partition_point(|layer| layer.base_ordinal <= ordinal)
                .checked_sub(1)
                .ok_or_else(|| {
                    crate::validation::corrupt_object("graph", "ordinal has no layer")
                })?;
            requests
                .entry(layer)
                .or_default()
                .push((index, ordinal.to_be_bytes().to_vec()));
        }
        for (layer, requests) in requests {
            let keys = requests
                .iter()
                .map(|(_, key)| key.clone())
                .collect::<Vec<_>>();
            let root = &self.roots(&self.descriptor.layers[layer])?.records;
            let values = self
                .lookup(store, layer, root, Kind::CommitRecords, &keys, work)
                .await?;
            for ((index, _), value) in requests.into_iter().zip(values) {
                let value = value.ok_or_else(|| {
                    crate::validation::corrupt_object("graph", "dense record is absent")
                })?;
                output[index] = Some(record(&value, ordinals[index], self.paths[layer].as_ref())?);
            }
        }
        Ok(output)
    }

    async fn lookup(
        &self,
        store: &Store,
        layer: usize,
        root: &PageRef,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut Work,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let mut output = vec![None; keys.len()];
        if keys.is_empty() {
            return Ok(output);
        }
        let mut pending = vec![(root.clone(), (0..keys.len()).collect::<Vec<_>>())];
        while !pending.is_empty() {
            for (reference, _) in &pending {
                reference.validate(kind, self.descriptor.layers[layer].bytes)?;
                if work.seen.insert((layer, reference.offset)) {
                    work.bytes = work
                        .bytes
                        .checked_add(u64::from(reference.bytes))
                        .ok_or_else(|| {
                            crate::validation::corrupt_object("graph", "query bytes overflow")
                        })?;
                    if work.bytes > self.max_bytes {
                        return corrupt("graph query exceeds byte bound");
                    }
                }
            }
            let batch = futures_util::stream::iter(std::mem::take(&mut pending))
                .map(|(reference, positions)| async move {
                    let page = self
                        .cache
                        .fetch(
                            store,
                            &self.paths[layer],
                            layer,
                            self.descriptor.layers[layer].bytes,
                            &reference,
                            kind,
                        )
                        .await?;
                    Ok::<_, MetadataError>((page, positions))
                })
                .buffered(4)
                .try_collect::<Vec<_>>()
                .await?;
            for (page, positions) in batch {
                match page.as_ref() {
                    Page::Leaf(rows) => {
                        for position in positions {
                            if let Ok(index) =
                                rows.binary_search_by(|(key, _)| key.cmp(&keys[position]))
                            {
                                output[position] = Some(rows[index].1.clone())
                            }
                        }
                    }
                    Page::Branch(children) => {
                        let mut batches = BTreeMap::<usize, Vec<usize>>::new();
                        for position in positions {
                            let index =
                                children.partition_point(|child| child.last < keys[position]);
                            if children
                                .get(index)
                                .is_some_and(|child| child.first <= keys[position])
                            {
                                batches.entry(index).or_default().push(position)
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
        Ok(output)
    }
}
#[cfg(test)]
#[path = "paged_tests.rs"]
mod tests;
