//! Generation-bound persistent path attribution.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    error::{MetadataError, Result},
    split_commit_graph::SplitCommitGraph,
    validation::validate_content_hash,
};

mod codec;
#[cfg(feature = "storage")]
mod storage;

use codec::{decode_layer, encode_layer};
#[cfg(feature = "storage")]
pub use storage::{
    load_path_state, load_path_state_checkpoint, load_path_state_checkpoint_record,
    load_path_state_descriptor, publish_path_state_checkpoint, upload_path_state,
};

const LAYER_MAGIC: &[u8; 8] = b"CRABPS02";
const LAYER_VERSION: u32 = 2;
const LAYER_HEADER_BYTES: usize = 24;
const RECORD_FIXED_BYTES: usize = 48;
const NODE_FIXED_BYTES: usize = 8;
const CHILD_FIXED_BYTES: usize = 12;
const MAX_AUTHOR_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_PATH_BYTES: usize = 1024 * 1024;
const MAX_PATH_COMPONENTS: usize = 1024;
const MAX_MUTATIONS_PER_COMMIT: usize = 10_000_000;
const MAX_CHILDREN_PER_NODE: usize = 10_000_000;
const MAX_LAYERS_BEFORE_COMPACTION: usize = 32;

/// Default aggregate descriptor and layer budget for one repository path-state index.
pub const DEFAULT_MAX_PATH_STATE_BYTES: u64 = 256 * 1024 * 1024;

/// One exact mutation applied to the first parent's persistent path trie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStateMutation {
    pub path: Vec<u8>,
    /// The path exists after this commit and receives this commit's ordinal.
    pub present: bool,
    /// Existing descendants are discarded before applying this mutation.
    pub reset: bool,
}

/// One verified commit and its exact first-parent path mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStateInput {
    pub oid: [u8; 20],
    pub first_parent: Option<[u8; 20]>,
    pub author: Vec<u8>,
    pub author_seconds: i64,
    pub message: Vec<u8>,
    pub mutations: Vec<PathStateMutation>,
}

/// Response metadata retained without reading the raw Git commit again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStateSummary {
    pub oid: [u8; 20],
    pub author: Vec<u8>,
    pub author_seconds: i64,
    pub message: Vec<u8>,
}

/// Stable location of one node in an immutable path-state layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathStateNodeRef {
    pub layer: u32,
    pub index: u32,
}

/// One positional commit record aligned with the split commit graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStateRecord {
    pub oid: [u8; 20],
    pub first_parent: Option<u32>,
    pub author: Vec<u8>,
    pub author_seconds: i64,
    pub message: Vec<u8>,
    pub root: PathStateNodeRef,
}

/// One immutable persistent trie node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStateNode {
    pub value: Option<u32>,
    pub children: BTreeMap<Vec<u8>, PathStateNodeRef>,
}

/// One immutable positional path-state layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStateLayer {
    pub base_ordinal: u32,
    pub records: Vec<PathStateRecord>,
    pub nodes: Vec<PathStateNode>,
}

/// Content-addressed layer reference in a path-state descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathStateLayerRef {
    pub hash: String,
    pub path: String,
    pub base_ordinal: u32,
    pub commit_count: u32,
    pub node_count: u32,
    pub bytes: u64,
}

/// Immutable generation-bound path-state descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathStateDescriptor {
    pub version: u32,
    pub generation: u64,
    pub pack_index_hash: String,
    pub git_validation_digest: String,
    pub commit_ordinal_digest: String,
    pub commit_count: u32,
    pub layers: Vec<PathStateLayerRef>,
}

/// Mutable locator for resumable generation-owner construction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathStateCheckpoint {
    pub version: u32,
    pub generation: u64,
    pub pack_index_hash: String,
    pub git_validation_digest: String,
    pub commit_ordinal_digest: String,
    pub commit_count: u32,
    pub descriptor_hash: String,
}

/// Complete validated persistent path-state index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStateIndex {
    pub descriptor: PathStateDescriptor,
    pub layers: Vec<PathStateLayer>,
}

/// One encoded immutable layer.
#[derive(Debug, Clone)]
pub struct PathStateLayerObject {
    pub reference: PathStateLayerRef,
    pub bytes: Vec<u8>,
}

/// Immutable objects that must be durable before descriptor publication.
#[derive(Debug, Clone)]
pub struct PathStateWrite {
    pub descriptor_hash: String,
    pub descriptor_bytes: Vec<u8>,
    pub layers: Vec<PathStateLayerObject>,
    index: PathStateIndex,
}

impl PathStateWrite {
    /// Return the number of commit roots covered by the descriptor.
    #[must_use]
    pub fn commit_count(&self) -> u32 {
        self.index.descriptor.commit_count
    }

    /// Consume uploaded bytes and retain the validated in-memory prefix.
    #[must_use]
    pub fn into_index(self) -> PathStateIndex {
        self.index
    }
}

impl PathStateIndex {
    /// Validate decoded layers against one exact positional commit graph.
    pub fn new(
        descriptor: PathStateDescriptor,
        layers: Vec<PathStateLayer>,
        graph: &SplitCommitGraph,
    ) -> Result<Self> {
        if descriptor.commit_count != graph.descriptor.commit_count {
            return corrupt("path-state descriptor is not complete");
        }
        Self::new_prefix(descriptor, layers, graph)
    }

    /// Validate a construction checkpoint against a commit-graph prefix.
    pub fn new_prefix(
        descriptor: PathStateDescriptor,
        layers: Vec<PathStateLayer>,
        graph: &SplitCommitGraph,
    ) -> Result<Self> {
        validate_descriptor(&descriptor, &layers)?;
        if descriptor.generation != graph.descriptor.generation
            || descriptor.pack_index_hash != graph.descriptor.pack_index_hash
            || descriptor.git_validation_digest != graph.descriptor.git_validation_digest
            || descriptor.commit_count > graph.descriptor.commit_count
            || descriptor.commit_ordinal_digest != graph.ordinal_digest()
        {
            return corrupt("path-state descriptor does not match its commit graph");
        }
        let index = Self { descriptor, layers };
        for ordinal in 0..index.descriptor.commit_count {
            let record = index
                .record(ordinal)
                .ok_or_else(|| corruption("path-state record is missing"))?;
            let commit = graph
                .record(ordinal)
                .ok_or_else(|| corruption("commit graph record is missing"))?;
            if record.oid != commit.oid || record.first_parent != commit.parents.first().copied() {
                return corrupt("path-state record does not match its commit ordinal");
            }
        }
        Ok(index)
    }

    /// Return one positional record.
    #[must_use]
    pub fn record(&self, ordinal: u32) -> Option<&PathStateRecord> {
        let layer_index = self
            .descriptor
            .layers
            .partition_point(|layer| layer.base_ordinal <= ordinal)
            .checked_sub(1)?;
        let reference = &self.descriptor.layers[layer_index];
        self.layers[layer_index]
            .records
            .get(ordinal.checked_sub(reference.base_ordinal)? as usize)
    }

    /// Resolve the exact last-change commit for each raw path at one commit.
    pub fn latest(
        &self,
        graph: &SplitCommitGraph,
        start: &[u8; 20],
        paths: &[&[u8]],
    ) -> Result<Vec<PathStateSummary>> {
        let start_ordinal = graph
            .ordinal(start)
            .ok_or_else(|| corruption("path-state start commit is absent from the graph"))?;
        let root = self
            .record(start_ordinal)
            .ok_or_else(|| corruption("path-state start record is missing"))?
            .root;
        paths
            .iter()
            .map(|path| {
                validate_path(path, false)?;
                let ordinal = self
                    .lookup(root, path)?
                    .ok_or_else(|| corruption("path-state has no value for a visible path"))?;
                if ordinal > start_ordinal {
                    return corrupt("path-state points to a future commit");
                }
                let record = self
                    .record(ordinal)
                    .ok_or_else(|| corruption("path-state summary record is missing"))?;
                Ok(PathStateSummary {
                    oid: record.oid,
                    author: record.author.clone(),
                    author_seconds: record.author_seconds,
                    message: record.message.clone(),
                })
            })
            .collect()
    }

    fn lookup(&self, root: PathStateNodeRef, path: &[u8]) -> Result<Option<u32>> {
        let mut current = root;
        for component in path.split(|byte| *byte == b'/') {
            let node = self.node(current)?;
            let Some(next) = node.children.get(component) else {
                return Ok(None);
            };
            current = *next;
        }
        Ok(self.node(current)?.value)
    }

    fn node(&self, reference: PathStateNodeRef) -> Result<&PathStateNode> {
        self.layers
            .get(reference.layer as usize)
            .and_then(|layer| layer.nodes.get(reference.index as usize))
            .ok_or_else(|| corruption("path-state node reference is out of bounds"))
    }
}

/// Append persistent trie roots for the commit-graph suffix missing from the base.
pub fn append_path_state(
    base: Option<PathStateIndex>,
    graph: &SplitCommitGraph,
    inputs: Vec<PathStateInput>,
) -> Result<PathStateWrite> {
    let base_count = base
        .as_ref()
        .map_or(0, |index| index.descriptor.commit_count);
    if base_count > graph.descriptor.commit_count {
        return corrupt("path-state base exceeds the commit graph");
    }
    if let Some(base) = &base {
        for ordinal in 0..base_count {
            let record = base
                .record(ordinal)
                .ok_or_else(|| corruption("path-state base record is missing"))?;
            let graph_record = graph
                .record(ordinal)
                .ok_or_else(|| corruption("path-state base commit is missing"))?;
            if record.oid != graph_record.oid
                || record.first_parent != graph_record.parents.first().copied()
            {
                return corrupt("path-state base does not match the commit graph prefix");
            }
        }
    }

    let mut by_oid = BTreeMap::new();
    for mut input in inputs {
        normalize_input(&mut input)?;
        if by_oid.insert(input.oid, input).is_some() {
            return corrupt("path-state inputs contain a duplicate commit");
        }
    }
    let target_count = base_count
        .checked_add(
            u32::try_from(by_oid.len())
                .map_err(|_| corruption("path-state input count overflows"))?,
        )
        .ok_or_else(|| corruption("path-state target count overflows"))?;
    if target_count > graph.descriptor.commit_count {
        return corrupt("path-state inputs exceed the commit graph");
    }

    let layer_ordinal = base.as_ref().map_or(0, |index| index.layers.len() as u32);
    let mut records = Vec::new();
    let mut nodes = Vec::new();
    for ordinal in base_count..target_count {
        let graph_record = graph
            .record(ordinal)
            .ok_or_else(|| corruption("path-state commit graph record is missing"))?;
        let input = by_oid
            .remove(&graph_record.oid)
            .ok_or_else(|| corruption("path-state input is incomplete"))?;
        let first_parent = graph_record.parents.first().copied();
        let expected_parent = first_parent
            .and_then(|parent| graph.record(parent))
            .map(|parent| parent.oid);
        if input.first_parent != expected_parent {
            return corrupt("path-state input first parent does not match the graph");
        }

        let parent_root = first_parent
            .and_then(|parent| record_from_parts(base.as_ref(), &records, base_count, parent))
            .map(|record| record.root);
        let mut root = match parent_root {
            Some(root) => {
                WorkingNode::from_node(node_from_parts(base.as_ref(), &nodes, layer_ordinal, root)?)
            }
            None => WorkingNode::default(),
        };
        for mutation in &input.mutations {
            apply_mutation(
                &mut root,
                mutation,
                ordinal,
                base.as_ref(),
                &nodes,
                layer_ordinal,
            )?;
        }
        let root = freeze_node(root, &mut nodes, layer_ordinal)?;
        records.push(PathStateRecord {
            oid: input.oid,
            first_parent,
            author: input.author,
            author_seconds: input.author_seconds,
            message: input.message,
            root,
        });
    }
    if !by_oid.is_empty() {
        return corrupt("path-state inputs contain commits outside the graph suffix");
    }

    let mut references = base
        .as_ref()
        .map(|index| index.descriptor.layers.clone())
        .unwrap_or_default();
    let mut layers = base.map_or_else(Vec::new, |index| index.layers);
    let mut objects = Vec::new();
    if !records.is_empty() {
        let layer = PathStateLayer {
            base_ordinal: base_count,
            records,
            nodes,
        };
        let bytes = encode_layer(&layer)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let reference = PathStateLayerRef {
            path: path_state_layer_path(&hash),
            hash,
            base_ordinal: layer.base_ordinal,
            commit_count: layer.records.len() as u32,
            node_count: layer.nodes.len() as u32,
            bytes: bytes.len() as u64,
        };
        references.push(reference.clone());
        objects.push(PathStateLayerObject { reference, bytes });
        layers.push(layer);
    }
    if layers.len() > MAX_LAYERS_BEFORE_COMPACTION {
        let layer = compact_layers(&layers)?;
        let bytes = encode_layer(&layer)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let reference = PathStateLayerRef {
            path: path_state_layer_path(&hash),
            hash,
            base_ordinal: 0,
            commit_count: layer.records.len() as u32,
            node_count: layer.nodes.len() as u32,
            bytes: bytes.len() as u64,
        };
        references = vec![reference.clone()];
        objects = vec![PathStateLayerObject { reference, bytes }];
        layers = vec![layer];
    }
    let descriptor = PathStateDescriptor {
        version: LAYER_VERSION,
        generation: graph.descriptor.generation,
        pack_index_hash: graph.descriptor.pack_index_hash.clone(),
        git_validation_digest: graph.descriptor.git_validation_digest.clone(),
        commit_ordinal_digest: graph.ordinal_digest(),
        commit_count: target_count,
        layers: references,
    };
    let index = PathStateIndex::new_prefix(descriptor.clone(), layers, graph)?;
    let descriptor_bytes = serde_json::to_vec(&descriptor).map_err(|source| {
        MetadataError::Internal(format!("path-state descriptor encode: {source}"))
    })?;
    Ok(PathStateWrite {
        descriptor_hash: blake3::hash(&descriptor_bytes).to_hex().to_string(),
        descriptor_bytes,
        layers: objects,
        index,
    })
}

fn compact_layers(layers: &[PathStateLayer]) -> Result<PathStateLayer> {
    let mut remapped = BTreeMap::new();
    let mut nodes = Vec::new();
    let mut records = Vec::new();
    for layer in layers {
        for record in &layer.records {
            let mut record = record.clone();
            record.root = copy_compacted_node(record.root, layers, &mut remapped, &mut nodes)?;
            records.push(record);
        }
    }
    Ok(PathStateLayer {
        base_ordinal: 0,
        records,
        nodes,
    })
}

fn copy_compacted_node(
    source: PathStateNodeRef,
    layers: &[PathStateLayer],
    remapped: &mut BTreeMap<PathStateNodeRef, PathStateNodeRef>,
    nodes: &mut Vec<PathStateNode>,
) -> Result<PathStateNodeRef> {
    if let Some(reference) = remapped.get(&source) {
        return Ok(*reference);
    }
    let node = layers
        .get(source.layer as usize)
        .and_then(|layer| layer.nodes.get(source.index as usize))
        .ok_or_else(|| corruption("path-state compaction source node is missing"))?;
    let mut children = BTreeMap::new();
    for (name, child) in &node.children {
        children.insert(
            name.clone(),
            copy_compacted_node(*child, layers, remapped, nodes)?,
        );
    }
    let reference = PathStateNodeRef {
        layer: 0,
        index: u32::try_from(nodes.len())
            .map_err(|_| corruption("compacted path-state has too many nodes"))?,
    };
    nodes.push(PathStateNode {
        value: node.value,
        children,
    });
    remapped.insert(source, reference);
    Ok(reference)
}

fn record_from_parts<'a>(
    base: Option<&'a PathStateIndex>,
    records: &'a [PathStateRecord],
    base_count: u32,
    ordinal: u32,
) -> Option<&'a PathStateRecord> {
    if ordinal < base_count {
        base?.record(ordinal)
    } else {
        records.get(ordinal.checked_sub(base_count)? as usize)
    }
}

fn node_from_parts<'a>(
    base: Option<&'a PathStateIndex>,
    nodes: &'a [PathStateNode],
    layer_ordinal: u32,
    reference: PathStateNodeRef,
) -> Result<&'a PathStateNode> {
    if reference.layer == layer_ordinal {
        return nodes
            .get(reference.index as usize)
            .ok_or_else(|| corruption("path-state current-layer node is missing"));
    }
    base.and_then(|index| index.layers.get(reference.layer as usize))
        .and_then(|layer| layer.nodes.get(reference.index as usize))
        .ok_or_else(|| corruption("path-state base node is missing"))
}

#[derive(Debug, Default)]
struct WorkingNode {
    value: Option<u32>,
    children: BTreeMap<Vec<u8>, WorkingChild>,
}

#[derive(Debug)]
enum WorkingChild {
    Shared(PathStateNodeRef),
    Owned(Box<WorkingNode>),
}

impl WorkingNode {
    fn from_node(node: &PathStateNode) -> Self {
        Self {
            value: node.value,
            children: node
                .children
                .iter()
                .map(|(name, reference)| (name.clone(), WorkingChild::Shared(*reference)))
                .collect(),
        }
    }
}

fn apply_mutation(
    root: &mut WorkingNode,
    mutation: &PathStateMutation,
    ordinal: u32,
    base: Option<&PathStateIndex>,
    nodes: &[PathStateNode],
    layer_ordinal: u32,
) -> Result<()> {
    if mutation.path.is_empty() {
        if mutation.reset {
            root.children.clear();
        }
        root.value = mutation.present.then_some(ordinal);
        return Ok(());
    }
    let components = mutation
        .path
        .split(|byte| *byte == b'/')
        .collect::<Vec<_>>();
    apply_components(
        root,
        &components,
        mutation,
        ordinal,
        base,
        nodes,
        layer_ordinal,
    )
}

fn apply_components(
    node: &mut WorkingNode,
    components: &[&[u8]],
    mutation: &PathStateMutation,
    ordinal: u32,
    base: Option<&PathStateIndex>,
    nodes: &[PathStateNode],
    layer_ordinal: u32,
) -> Result<()> {
    let name = components
        .first()
        .ok_or_else(|| corruption("path-state mutation has no component"))?;
    if components.len() == 1 {
        if !mutation.present {
            node.children.remove(*name);
            return Ok(());
        }
        let mut child = if mutation.reset {
            WorkingNode::default()
        } else {
            take_working_child(node.children.remove(*name), base, nodes, layer_ordinal)?
        };
        child.value = Some(ordinal);
        node.children
            .insert(name.to_vec(), WorkingChild::Owned(Box::new(child)));
        return Ok(());
    }

    let mut child = take_working_child(node.children.remove(*name), base, nodes, layer_ordinal)?;
    apply_components(
        &mut child,
        &components[1..],
        mutation,
        ordinal,
        base,
        nodes,
        layer_ordinal,
    )?;
    node.children
        .insert(name.to_vec(), WorkingChild::Owned(Box::new(child)));
    Ok(())
}

fn take_working_child(
    child: Option<WorkingChild>,
    base: Option<&PathStateIndex>,
    nodes: &[PathStateNode],
    layer_ordinal: u32,
) -> Result<WorkingNode> {
    match child {
        Some(WorkingChild::Owned(node)) => Ok(*node),
        Some(WorkingChild::Shared(reference)) => Ok(WorkingNode::from_node(node_from_parts(
            base,
            nodes,
            layer_ordinal,
            reference,
        )?)),
        None => Ok(WorkingNode::default()),
    }
}

fn freeze_node(
    node: WorkingNode,
    nodes: &mut Vec<PathStateNode>,
    layer_ordinal: u32,
) -> Result<PathStateNodeRef> {
    let mut children = BTreeMap::new();
    for (name, child) in node.children {
        let reference = match child {
            WorkingChild::Shared(reference) => reference,
            WorkingChild::Owned(child) => freeze_node(*child, nodes, layer_ordinal)?,
        };
        children.insert(name, reference);
    }
    let index = u32::try_from(nodes.len())
        .map_err(|_| corruption("path-state layer has too many nodes"))?;
    nodes.push(PathStateNode {
        value: node.value,
        children,
    });
    Ok(PathStateNodeRef {
        layer: layer_ordinal,
        index,
    })
}

fn normalize_input(input: &mut PathStateInput) -> Result<()> {
    if input.author.len() > MAX_AUTHOR_BYTES || input.message.len() > MAX_MESSAGE_BYTES {
        return corrupt("path-state commit summary exceeds its byte limit");
    }
    let mut mutations = BTreeMap::new();
    for mutation in input.mutations.drain(..) {
        validate_path(&mutation.path, true)?;
        match mutations.entry(mutation.path.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(mutation);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let current = entry.get_mut();
                current.present = mutation.present;
                current.reset |= mutation.reset;
            }
        }
    }
    if mutations.len() > MAX_MUTATIONS_PER_COMMIT {
        return corrupt("path-state commit has too many mutations");
    }
    input.mutations = mutations.into_values().collect();
    Ok(())
}

fn validate_path(path: &[u8], allow_root: bool) -> Result<()> {
    if path.len() > MAX_PATH_BYTES
        || path.contains(&0)
        || (!path.is_empty() && path.split(|byte| *byte == b'/').any(<[u8]>::is_empty))
        || path.split(|byte| *byte == b'/').count() > MAX_PATH_COMPONENTS
        || (path.is_empty() && !allow_root)
    {
        return corrupt("path-state mutation contains an invalid Git path");
    }
    Ok(())
}

fn validate_descriptor(descriptor: &PathStateDescriptor, layers: &[PathStateLayer]) -> Result<()> {
    validate_descriptor_shape(descriptor)?;
    if descriptor.layers.len() != layers.len() {
        return corrupt("path-state descriptor layer count does not match loaded layers");
    }
    for (layer_ordinal, (reference, layer)) in descriptor.layers.iter().zip(layers).enumerate() {
        if reference.base_ordinal != layer.base_ordinal
            || reference.commit_count as usize != layer.records.len()
            || reference.node_count as usize != layer.nodes.len()
        {
            return corrupt("path-state layer does not match its descriptor reference");
        }
        for record in &layer.records {
            validate_node_ref(record.root, layer_ordinal, layers)?;
            if record.author.len() > MAX_AUTHOR_BYTES || record.message.len() > MAX_MESSAGE_BYTES {
                return corrupt("path-state record exceeds its summary limits");
            }
        }
        for (node_index, node) in layer.nodes.iter().enumerate() {
            if node
                .value
                .is_some_and(|value| value >= descriptor.commit_count)
                || node.children.len() > MAX_CHILDREN_PER_NODE
            {
                return corrupt("path-state node exceeds its limits");
            }
            for (name, child) in &node.children {
                if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
                    return corrupt("path-state node has an invalid child name");
                }
                validate_node_ref(*child, layer_ordinal, layers)?;
                if child.layer == layer_ordinal as u32 && child.index as usize >= node_index {
                    return corrupt("path-state node graph is not acyclic");
                }
            }
        }
    }
    Ok(())
}

fn validate_node_ref(
    reference: PathStateNodeRef,
    maximum_layer: usize,
    layers: &[PathStateLayer],
) -> Result<()> {
    if reference.layer as usize > maximum_layer
        || layers
            .get(reference.layer as usize)
            .and_then(|layer| layer.nodes.get(reference.index as usize))
            .is_none()
    {
        return corrupt("path-state node reference is out of bounds");
    }
    Ok(())
}

fn validate_descriptor_shape(descriptor: &PathStateDescriptor) -> Result<()> {
    if descriptor.version != LAYER_VERSION {
        return corrupt("unsupported path-state descriptor version");
    }
    validate_content_hash(
        &descriptor.pack_index_hash,
        "path-state pack index hash",
        "path-state descriptor",
    )?;
    validate_content_hash(
        &descriptor.git_validation_digest,
        "path-state Git validation digest",
        "path-state descriptor",
    )?;
    validate_content_hash(
        &descriptor.commit_ordinal_digest,
        "path-state commit ordinal digest",
        "path-state descriptor",
    )?;
    let mut next = 0u32;
    for reference in &descriptor.layers {
        validate_content_hash(
            &reference.hash,
            "path-state layer hash",
            "path-state descriptor",
        )?;
        if reference.path != path_state_layer_path(&reference.hash)
            || reference.base_ordinal != next
            || reference.commit_count == 0
            || reference.node_count == 0
            || reference.bytes < LAYER_HEADER_BYTES as u64
        {
            return corrupt("invalid path-state layer reference");
        }
        next = next
            .checked_add(reference.commit_count)
            .ok_or_else(|| corruption("path-state commit count overflows"))?;
    }
    if next != descriptor.commit_count {
        return corrupt("path-state descriptor commit count does not match its layers");
    }
    Ok(())
}

/// Return the immutable path for one encoded layer.
#[must_use]
pub fn path_state_layer_path(hash: &str) -> String {
    format!("metadata/path-state/layers/{hash}.bin")
}

/// Return the mutable construction-checkpoint path for one Git generation.
#[must_use]
pub fn path_state_checkpoint_path(git_validation_digest: &str) -> String {
    format!("metadata/path-state/work/{git_validation_digest}.json")
}

fn corruption(reason: &str) -> MetadataError {
    MetadataError::CorruptObject {
        path: "path-state".to_owned(),
        reason: reason.to_owned(),
    }
}

fn corrupt<T>(reason: &str) -> Result<T> {
    Err(corruption(reason))
}

fn corrupt_at<T>(path: &str, reason: &str) -> Result<T> {
    Err(MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_commit_graph::{
        CommitGraphDescriptor, CommitGraphLayer, CommitGraphLayerRef, CommitGraphRecord,
    };

    fn graph() -> SplitCommitGraph {
        let layer = CommitGraphLayer {
            base_ordinal: 0,
            records: vec![
                CommitGraphRecord {
                    oid: [1; 20],
                    tree_oid: [11; 20],
                    commit_time: 1,
                    corrected_generation: 1,
                    parents: vec![],
                },
                CommitGraphRecord {
                    oid: [2; 20],
                    tree_oid: [12; 20],
                    commit_time: 2,
                    corrected_generation: 2,
                    parents: vec![0],
                },
            ],
        };
        SplitCommitGraph::new(
            CommitGraphDescriptor {
                version: 1,
                generation: 7,
                pack_index_hash: "a".repeat(64),
                git_validation_digest: "b".repeat(64),
                commit_count: 2,
                layers: vec![CommitGraphLayerRef {
                    hash: "c".repeat(64),
                    path: format!("metadata/commit-graph/layers/{}.bin", "c".repeat(64)),
                    base_ordinal: 0,
                    commit_count: 2,
                    bytes: 24,
                }],
            },
            vec![layer],
        )
        .expect("graph")
    }

    fn decode_write(write: &PathStateWrite, graph: &SplitCommitGraph) -> PathStateIndex {
        let descriptor: PathStateDescriptor =
            serde_json::from_slice(&write.descriptor_bytes).expect("descriptor");
        let layers = write
            .layers
            .iter()
            .map(|layer| decode_layer(&layer.bytes, &layer.reference, "layer").expect("layer"))
            .collect();
        PathStateIndex::new(descriptor, layers, graph).expect("index")
    }

    #[test]
    fn persistent_path_state_resolves_exact_values_without_history_scan() {
        let graph = graph();
        let write = append_path_state(
            None,
            &graph,
            vec![
                PathStateInput {
                    oid: [1; 20],
                    first_parent: None,
                    author: b"Root".to_vec(),
                    author_seconds: 1,
                    message: b"root".to_vec(),
                    mutations: vec![
                        PathStateMutation {
                            path: Vec::new(),
                            present: false,
                            reset: true,
                        },
                        PathStateMutation {
                            path: b"README.md".to_vec(),
                            present: true,
                            reset: false,
                        },
                        PathStateMutation {
                            path: b"src".to_vec(),
                            present: true,
                            reset: false,
                        },
                        PathStateMutation {
                            path: b"src/lib.rs".to_vec(),
                            present: true,
                            reset: false,
                        },
                        PathStateMutation {
                            path: b"raw/\xff.rs".to_vec(),
                            present: true,
                            reset: false,
                        },
                    ],
                },
                PathStateInput {
                    oid: [2; 20],
                    first_parent: Some([1; 20]),
                    author: b"Head".to_vec(),
                    author_seconds: 2,
                    message: b"head".to_vec(),
                    mutations: vec![PathStateMutation {
                        path: b"src/lib.rs".to_vec(),
                        present: true,
                        reset: false,
                    }],
                },
            ],
        )
        .expect("write");
        let index = decode_write(&write, &graph);
        let summaries = index
            .latest(&graph, &[2; 20], &[b"src/lib.rs", b"src", b"README.md"])
            .expect("latest");
        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.oid)
                .collect::<Vec<_>>(),
            vec![[2; 20], [1; 20], [1; 20]]
        );
        assert_eq!(
            index
                .latest(&graph, &[2; 20], &[b"raw/\xff.rs"])
                .expect("raw path")[0]
                .oid,
            [1; 20]
        );
    }

    #[test]
    fn reset_replaces_a_subtree_without_changing_an_old_commit_root() {
        let graph = graph();
        let write = append_path_state(
            None,
            &graph,
            vec![
                PathStateInput {
                    oid: [1; 20],
                    first_parent: None,
                    author: Vec::new(),
                    author_seconds: 1,
                    message: Vec::new(),
                    mutations: vec![PathStateMutation {
                        path: b"src/old.rs".to_vec(),
                        present: true,
                        reset: false,
                    }],
                },
                PathStateInput {
                    oid: [2; 20],
                    first_parent: Some([1; 20]),
                    author: Vec::new(),
                    author_seconds: 2,
                    message: Vec::new(),
                    mutations: vec![
                        PathStateMutation {
                            path: b"src".to_vec(),
                            present: true,
                            reset: true,
                        },
                        PathStateMutation {
                            path: b"src/new.rs".to_vec(),
                            present: true,
                            reset: false,
                        },
                    ],
                },
            ],
        )
        .expect("write");
        let index = decode_write(&write, &graph);
        assert_eq!(
            index
                .latest(&graph, &[1; 20], &[b"src/old.rs"])
                .expect("old root")[0]
                .oid,
            [1; 20]
        );
        assert!(
            index
                .lookup(index.record(1).expect("record").root, b"src/old.rs")
                .expect("lookup")
                .is_none()
        );
        assert_eq!(
            index
                .latest(&graph, &[2; 20], &[b"src/new.rs"])
                .expect("new root")[0]
                .oid,
            [2; 20]
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn checkpoint_round_trip_resumes_a_verified_graph_prefix() {
        use std::sync::Arc;

        use object_store::memory::InMemory;

        let graph = graph();
        let store = crab_storage::Store::new(Arc::new(InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "org/repo".to_owned());
        let write = append_path_state(
            None,
            &graph,
            vec![PathStateInput {
                oid: [1; 20],
                first_parent: None,
                author: b"Root".to_vec(),
                author_seconds: 1,
                message: b"root".to_vec(),
                mutations: vec![PathStateMutation {
                    path: b"README.md".to_vec(),
                    present: true,
                    reset: false,
                }],
            }],
        )
        .expect("prefix");
        upload_path_state(&store, &layout, &write)
            .await
            .expect("upload");
        publish_path_state_checkpoint(
            &store,
            &layout,
            &graph,
            &write.descriptor_hash,
            write.commit_count(),
            None,
        )
        .await
        .expect("publish checkpoint");

        let resumed =
            load_path_state_checkpoint(&store, &layout, &graph, DEFAULT_MAX_PATH_STATE_BYTES)
                .await
                .expect("load checkpoint")
                .expect("checkpoint exists");

        assert_eq!(resumed.descriptor.commit_count, 1);

        let replacement_hash = "e".repeat(64);
        publish_path_state_checkpoint(&store, &layout, &graph, &replacement_hash, 0, None)
            .await
            .expect("monotonic no-op");
        let checkpoint = load_path_state_checkpoint_record(
            &store,
            &layout,
            &graph.descriptor.git_validation_digest,
        )
        .await
        .expect("checkpoint record")
        .expect("checkpoint exists");
        assert_eq!(checkpoint.descriptor_hash, write.descriptor_hash);

        publish_path_state_checkpoint(
            &store,
            &layout,
            &graph,
            &replacement_hash,
            0,
            Some(&write.descriptor_hash),
        )
        .await
        .expect("replace corrupt checkpoint");
        let checkpoint = load_path_state_checkpoint_record(
            &store,
            &layout,
            &graph.descriptor.git_validation_digest,
        )
        .await
        .expect("replacement record")
        .expect("checkpoint exists");
        assert_eq!(checkpoint.descriptor_hash, replacement_hash);
    }
}
