//! Complete, generation-bound Git commit graph stored as immutable binary layers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

#[cfg(feature = "storage")]
use bytes::Bytes;
#[cfg(feature = "storage")]
use crab_storage::{StorageError, Store, StoreLayout};

const LAYER_MAGIC: &[u8; 8] = b"CRABCG01";
const LAYER_VERSION: u32 = 1;
const LAYER_HEADER_BYTES: usize = 24;
const RECORD_FIXED_BYTES: usize = 60;
const MAX_PARENTS: usize = 1_024;

/// Default aggregate descriptor and layer budget for one repository graph.
pub const DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES: u64 = 128 * 1024 * 1024;

/// One commit discovered from a verified local Git object database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphInput {
    pub oid: [u8; 20],
    pub tree_oid: [u8; 20],
    pub commit_time: i64,
    pub parents: Vec<[u8; 20]>,
}

/// One immutable positional commit-graph record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphRecord {
    pub oid: [u8; 20],
    pub tree_oid: [u8; 20],
    pub commit_time: i64,
    /// Git corrected commit date: max(commit time, parent value + 1).
    pub corrected_generation: u64,
    pub parents: Vec<u32>,
}

/// One decoded immutable layer. Ordinals are `base_ordinal + record index`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphLayer {
    pub base_ordinal: u32,
    pub records: Vec<CommitGraphRecord>,
}

/// Content-addressed layer reference in a graph descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitGraphLayerRef {
    pub hash: String,
    pub path: String,
    pub base_ordinal: u32,
    pub commit_count: u32,
    pub bytes: u64,
}

/// Small immutable object named by `Manifest::commit_graph_hash`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitGraphDescriptor {
    pub version: u32,
    pub generation: u64,
    pub pack_index_hash: String,
    pub git_validation_digest: String,
    pub commit_count: u32,
    pub layers: Vec<CommitGraphLayerRef>,
}

/// A validated complete split graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitCommitGraph {
    pub descriptor: CommitGraphDescriptor,
    pub layers: Vec<CommitGraphLayer>,
    oid_to_ordinal: HashMap<[u8; 20], u32>,
}

/// One new immutable layer object produced by append or compaction.
#[derive(Debug, Clone)]
pub struct CommitGraphLayerObject {
    pub reference: CommitGraphLayerRef,
    pub bytes: Vec<u8>,
}

/// Immutable objects that must be durable before publishing the descriptor hash.
#[derive(Debug, Clone)]
pub struct CommitGraphWrite {
    pub descriptor_hash: String,
    pub descriptor_bytes: Vec<u8>,
    pub layers: Vec<CommitGraphLayerObject>,
}

impl SplitCommitGraph {
    /// Validate and index a descriptor and its decoded layers.
    pub fn new(descriptor: CommitGraphDescriptor, layers: Vec<CommitGraphLayer>) -> Result<Self> {
        validate_descriptor(&descriptor, &layers)?;
        let mut oid_to_ordinal = HashMap::new();
        oid_to_ordinal
            .try_reserve(descriptor.commit_count as usize)
            .map_err(|source| {
                MetadataError::Internal(format!("commit graph allocation: {source}"))
            })?;
        let mut records = Vec::with_capacity(descriptor.commit_count as usize);
        for layer in &layers {
            for record in &layer.records {
                let ordinal = u32::try_from(records.len()).map_err(|_| {
                    MetadataError::Internal("commit graph exceeds u32 ordinals".to_owned())
                })?;
                if oid_to_ordinal.insert(record.oid, ordinal).is_some() {
                    return corrupt("commit graph contains duplicate commit OIDs");
                }
                records.push(record);
            }
        }
        for (ordinal, record) in records.iter().enumerate() {
            let expected = record
                .parents
                .iter()
                .map(|parent| {
                    if (*parent as usize) >= ordinal {
                        return corrupt("commit graph parent is not earlier than its child");
                    }
                    Ok(records[*parent as usize]
                        .corrected_generation
                        .saturating_add(1))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .max()
                .unwrap_or(0)
                .max(non_negative_time(record.commit_time));
            if record.corrected_generation != expected {
                return corrupt("commit graph corrected generation is invalid");
            }
        }
        Ok(Self {
            descriptor,
            layers,
            oid_to_ordinal,
        })
    }

    #[must_use]
    pub fn contains(&self, oid: &[u8; 20]) -> bool {
        self.oid_to_ordinal.contains_key(oid)
    }

    #[must_use]
    pub fn ordinal(&self, oid: &[u8; 20]) -> Option<u32> {
        self.oid_to_ordinal.get(oid).copied()
    }

    #[must_use]
    pub fn record(&self, ordinal: u32) -> Option<&CommitGraphRecord> {
        let layer_index = self
            .descriptor
            .layers
            .partition_point(|layer| layer.base_ordinal <= ordinal)
            .checked_sub(1)?;
        let layer_ref = &self.descriptor.layers[layer_index];
        self.layers[layer_index]
            .records
            .get(ordinal.saturating_sub(layer_ref.base_ordinal) as usize)
    }

    /// Return an exact ancestry answer when both commits are in this complete graph.
    #[must_use]
    pub fn is_ancestor(&self, ancestor: &[u8; 20], descendant: &[u8; 20]) -> Option<bool> {
        self.is_ancestor_with_limit(ancestor, descendant, usize::MAX)
    }

    /// Return an exact answer unless the positional walk reaches `max_commits`.
    #[must_use]
    pub fn is_ancestor_with_limit(
        &self,
        ancestor: &[u8; 20],
        descendant: &[u8; 20],
        max_commits: usize,
    ) -> Option<bool> {
        if max_commits == 0 {
            return None;
        }
        let ancestor = self.ordinal(ancestor)?;
        let descendant = self.ordinal(descendant)?;
        if ancestor == descendant {
            return Some(true);
        }
        let target_generation = self.record(ancestor)?.corrected_generation;
        let mut pending = vec![descendant];
        let mut seen = HashSet::new();
        while let Some(ordinal) = pending.pop() {
            if !seen.insert(ordinal) {
                continue;
            }
            if seen.len() > max_commits {
                return None;
            }
            let record = self.record(ordinal)?;
            if record.corrected_generation < target_generation {
                continue;
            }
            for parent in &record.parents {
                if *parent == ancestor {
                    return Some(true);
                }
                pending.push(*parent);
            }
        }
        Some(false)
    }

    /// Compute Git-compatible shallow boundaries; tips are depth one.
    #[must_use]
    pub fn shallow_boundary(&self, tips: &[[u8; 20]], depth: u32) -> Option<Vec<[u8; 20]>> {
        if depth == 0 {
            return tips
                .iter()
                .map(|tip| self.contains(tip).then_some(*tip))
                .collect();
        }
        let mut pending = VecDeque::new();
        let mut visited = HashMap::<u32, u32>::new();
        for tip in tips {
            let ordinal = self.ordinal(tip)?;
            if visited.insert(ordinal, 1).is_none() {
                pending.push_back((ordinal, 1));
            }
        }
        let mut boundary = Vec::new();
        while let Some((ordinal, current_depth)) = pending.pop_front() {
            let record = self.record(ordinal)?;
            if current_depth == depth {
                boundary.push(record.oid);
                continue;
            }
            for parent in &record.parents {
                let next_depth = current_depth.saturating_add(1);
                if visited
                    .get(parent)
                    .is_some_and(|known| *known <= next_depth)
                {
                    continue;
                }
                visited.insert(*parent, next_depth);
                pending.push_back((*parent, next_depth));
            }
        }
        boundary.sort_unstable();
        boundary.dedup();
        Some(boundary)
    }

    /// Return commit OIDs reachable from `tips` without crossing `boundary`.
    #[must_use]
    pub fn reachable_to_boundary(
        &self,
        tips: &[[u8; 20]],
        boundary: &[[u8; 20]],
    ) -> Option<HashSet<[u8; 20]>> {
        let boundary = boundary
            .iter()
            .map(|oid| self.ordinal(oid))
            .collect::<Option<HashSet<_>>>()?;
        let mut pending = tips
            .iter()
            .map(|oid| self.ordinal(oid))
            .collect::<Option<Vec<_>>>()?;
        let mut reachable = HashSet::new();
        while let Some(ordinal) = pending.pop() {
            if !reachable.insert(ordinal) || boundary.contains(&ordinal) {
                continue;
            }
            pending.extend(self.record(ordinal)?.parents.iter().copied());
        }
        reachable
            .into_iter()
            .map(|ordinal| self.record(ordinal).map(|record| record.oid))
            .collect()
    }

    /// Parse a hexadecimal SHA-1 and return whether it is in this graph.
    #[must_use]
    pub fn contains_hex(&self, oid: &str) -> bool {
        parse_sha1_hex(oid).is_some_and(|oid| self.contains(&oid))
    }

    /// Compute a shallow boundary from hexadecimal SHA-1 tips.
    #[must_use]
    pub fn shallow_boundary_hex(&self, tips: &[String], depth: u32) -> Option<Vec<String>> {
        let tips = tips
            .iter()
            .map(|oid| parse_sha1_hex(oid))
            .collect::<Option<Vec<_>>>()?;
        self.shallow_boundary(&tips, depth)
            .map(|boundary| boundary.iter().map(sha1_hex).collect::<Vec<_>>())
    }

    /// Return hexadecimal commit OIDs reachable without crossing `boundary`.
    #[must_use]
    pub fn reachable_to_boundary_hex(
        &self,
        tips: &[String],
        boundary: &[String],
    ) -> Option<HashSet<String>> {
        let tips = tips
            .iter()
            .map(|oid| parse_sha1_hex(oid))
            .collect::<Option<Vec<_>>>()?;
        let boundary = boundary
            .iter()
            .map(|oid| parse_sha1_hex(oid))
            .collect::<Option<Vec<_>>>()?;
        self.reachable_to_boundary(&tips, &boundary)
            .map(|reachable| reachable.iter().map(sha1_hex).collect::<HashSet<_>>())
    }

    /// Return whether `want` is reachable from any hexadecimal root.
    #[must_use]
    pub fn is_reachable_from_hex(&self, roots: &[String], want: &str) -> Option<bool> {
        let want = parse_sha1_hex(want)?;
        roots.iter().try_fold(false, |reachable, root| {
            if reachable {
                return Some(true);
            }
            let root = parse_sha1_hex(root)?;
            self.is_ancestor(&want, &root)
        })
    }

    fn records(&self) -> impl Iterator<Item = &CommitGraphRecord> {
        self.layers.iter().flat_map(|layer| layer.records.iter())
    }
}

/// Append commits and return the immutable objects needed for publication.
///
/// Returns `None` when the supplied roots cannot be proven complete from the
/// base plus additions. Callers must then omit graph acceleration.
pub fn append_split_commit_graph(
    base: Option<SplitCommitGraph>,
    generation: u64,
    pack_index_hash: String,
    git_validation_digest: String,
    roots: &[[u8; 20]],
    additions: Vec<CommitGraphInput>,
) -> Result<Option<CommitGraphWrite>> {
    update_split_commit_graph(
        base,
        generation,
        pack_index_hash,
        git_validation_digest,
        roots,
        additions,
        false,
    )
}

/// Geometrically compact adjacent layers without changing commit ordinals.
pub fn compact_split_commit_graph(graph: SplitCommitGraph) -> Result<Option<CommitGraphWrite>> {
    let descriptor = graph.descriptor.clone();
    update_split_commit_graph(
        Some(graph),
        descriptor.generation,
        descriptor.pack_index_hash,
        descriptor.git_validation_digest,
        &[],
        Vec::new(),
        true,
    )
}

/// Rebind unchanged graph layers to a new manifest pack identity.
pub fn rebind_split_commit_graph(
    graph: &SplitCommitGraph,
    generation: u64,
    pack_index_hash: String,
    git_validation_digest: String,
) -> Result<CommitGraphWrite> {
    let descriptor = CommitGraphDescriptor {
        version: LAYER_VERSION,
        generation,
        pack_index_hash,
        git_validation_digest,
        commit_count: graph.descriptor.commit_count,
        layers: graph.descriptor.layers.clone(),
    };
    SplitCommitGraph::new(descriptor.clone(), graph.layers.clone())?;
    let descriptor_bytes = encode_commit_graph_descriptor(&descriptor)?;
    Ok(CommitGraphWrite {
        descriptor_hash: blake3::hash(&descriptor_bytes).to_hex().to_string(),
        descriptor_bytes,
        layers: Vec::new(),
    })
}

fn update_split_commit_graph(
    base: Option<SplitCommitGraph>,
    generation: u64,
    pack_index_hash: String,
    git_validation_digest: String,
    roots: &[[u8; 20]],
    additions: Vec<CommitGraphInput>,
    compact_layers: bool,
) -> Result<Option<CommitGraphWrite>> {
    validate_content_hash(
        &pack_index_hash,
        "commit graph pack-index hash",
        "commit graph",
    )?;
    validate_content_hash(
        &git_validation_digest,
        "commit graph validation digest",
        "commit graph",
    )?;
    let mut records = base
        .as_ref()
        .map(|graph| graph.records().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut oid_to_ordinal = records
        .iter()
        .enumerate()
        .map(|(ordinal, record)| (record.oid, ordinal as u32))
        .collect::<HashMap<_, _>>();

    let mut pending = BTreeMap::new();
    for input in additions {
        if input.parents.len() > MAX_PARENTS {
            return corrupt("commit graph record has too many parents");
        }
        if oid_to_ordinal.contains_key(&input.oid) {
            continue;
        }
        match pending.entry(input.oid) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(input);
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &input => {
                return corrupt("commit graph additions disagree for one OID");
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    if pending.values().any(|input| {
        input
            .parents
            .iter()
            .any(|parent| !oid_to_ordinal.contains_key(parent) && !pending.contains_key(parent))
    }) {
        return Ok(None);
    }

    let mut children = HashMap::<[u8; 20], Vec<[u8; 20]>>::new();
    let mut indegree = HashMap::<[u8; 20], usize>::new();
    for input in pending.values() {
        let count = input
            .parents
            .iter()
            .filter(|parent| pending.contains_key(*parent))
            .count();
        indegree.insert(input.oid, count);
        for parent in input
            .parents
            .iter()
            .filter(|parent| pending.contains_key(*parent))
        {
            children.entry(*parent).or_default().push(input.oid);
        }
    }
    let mut ready = indegree
        .iter()
        .filter_map(|(oid, degree)| (*degree == 0).then_some(*oid))
        .collect::<BTreeSet<_>>();
    let first_new_ordinal = records.len();
    while let Some(oid) = ready.pop_first() {
        let input = pending.get(&oid).ok_or_else(|| {
            MetadataError::Internal("commit graph ready node disappeared".to_owned())
        })?;
        let parents = input
            .parents
            .iter()
            .map(|parent| {
                oid_to_ordinal.get(parent).copied().ok_or_else(|| {
                    MetadataError::Internal("commit graph parent ordinal disappeared".to_owned())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let corrected_generation = parents
            .iter()
            .filter_map(|parent| records.get(*parent as usize))
            .map(|parent| parent.corrected_generation.saturating_add(1))
            .max()
            .unwrap_or(0)
            .max(non_negative_time(input.commit_time));
        let ordinal = u32::try_from(records.len())
            .map_err(|_| MetadataError::Internal("commit graph exceeds u32 ordinals".to_owned()))?;
        records.push(CommitGraphRecord {
            oid,
            tree_oid: input.tree_oid,
            commit_time: input.commit_time,
            corrected_generation,
            parents,
        });
        oid_to_ordinal.insert(oid, ordinal);
        if let Some(successors) = children.get(&oid) {
            for successor in successors {
                let degree = indegree.get_mut(successor).ok_or_else(|| {
                    MetadataError::Internal("commit graph child degree disappeared".to_owned())
                })?;
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    ready.insert(*successor);
                }
            }
        }
    }
    if records.len().saturating_sub(first_new_ordinal) != pending.len() {
        return corrupt("commit graph additions contain a cycle");
    }
    if roots.iter().any(|root| !oid_to_ordinal.contains_key(root)) {
        return Ok(None);
    }

    let existing_layer_refs = base
        .as_ref()
        .map(|graph| graph.descriptor.layers.clone())
        .unwrap_or_default();
    let mut layers = base.map_or_else(Vec::new, |graph| graph.layers);
    if first_new_ordinal < records.len() {
        layers.push(CommitGraphLayer {
            base_ordinal: first_new_ordinal as u32,
            records: records[first_new_ordinal..].to_vec(),
        });
    }
    let mut changed_layers = Vec::new();
    if first_new_ordinal < records.len() {
        changed_layers.push(layers.len() - 1);
    }
    while compact_layers && layers.len() >= 2 {
        let newer = layers.last().map_or(0, |layer| layer.records.len());
        let older = layers[layers.len() - 2].records.len();
        if newer < older {
            break;
        }
        let newer = layers
            .pop()
            .ok_or_else(|| MetadataError::Internal("missing graph layer".to_owned()))?;
        let mut older = layers
            .pop()
            .ok_or_else(|| MetadataError::Internal("missing graph layer".to_owned()))?;
        older.records.extend(newer.records);
        layers.push(older);
        changed_layers.retain(|index| *index < layers.len() - 1);
        changed_layers.push(layers.len() - 1);
    }

    if compact_layers && changed_layers.is_empty() {
        return Ok(None);
    }

    let mut layer_refs = Vec::with_capacity(layers.len());
    let mut layer_objects = Vec::new();
    for (index, layer) in layers.iter().enumerate() {
        if !changed_layers.contains(&index)
            && let Some(existing) = base_layer_ref_at(layer, index, &existing_layer_refs)
        {
            layer_refs.push(existing.clone());
            continue;
        }
        let bytes = encode_commit_graph_layer(layer)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let reference = CommitGraphLayerRef {
            path: format!("metadata/commit-graph/layers/{hash}.bin"),
            hash,
            base_ordinal: layer.base_ordinal,
            commit_count: layer.records.len() as u32,
            bytes: bytes.len() as u64,
        };
        layer_refs.push(reference.clone());
        layer_objects.push(CommitGraphLayerObject { reference, bytes });
    }
    let descriptor = CommitGraphDescriptor {
        version: LAYER_VERSION,
        generation,
        pack_index_hash,
        git_validation_digest,
        commit_count: records.len() as u32,
        layers: layer_refs,
    };
    let graph = SplitCommitGraph::new(descriptor.clone(), layers)?;
    if roots.iter().any(|root| !graph.contains(root)) {
        return Ok(None);
    }
    let descriptor_bytes = encode_commit_graph_descriptor(&descriptor)?;
    let descriptor_hash = blake3::hash(&descriptor_bytes).to_hex().to_string();
    Ok(Some(CommitGraphWrite {
        descriptor_hash,
        descriptor_bytes,
        layers: layer_objects,
    }))
}

fn base_layer_ref_at<'a>(
    layer: &CommitGraphLayer,
    index: usize,
    references: &'a [CommitGraphLayerRef],
) -> Option<&'a CommitGraphLayerRef> {
    let reference = references.get(index)?;
    (reference.base_ordinal == layer.base_ordinal
        && reference.commit_count as usize == layer.records.len())
    .then_some(reference)
}

#[must_use]
pub fn commit_graph_layer_path(hash: &str) -> String {
    format!("metadata/commit-graph/layers/{hash}.bin")
}

pub fn encode_commit_graph_descriptor(descriptor: &CommitGraphDescriptor) -> Result<Vec<u8>> {
    serde_json::to_vec(descriptor).map_err(|source| {
        MetadataError::Internal(format!("commit graph descriptor encode: {source}"))
    })
}

pub fn decode_commit_graph_descriptor(bytes: &[u8], path: &str) -> Result<CommitGraphDescriptor> {
    let descriptor =
        serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: format!("invalid commit graph descriptor: {source}"),
        })?;
    validate_descriptor_shape(&descriptor, path)?;
    Ok(descriptor)
}

pub fn encode_commit_graph_layer(layer: &CommitGraphLayer) -> Result<Vec<u8>> {
    let parent_count = layer.records.iter().try_fold(0usize, |count, record| {
        if record.parents.len() > MAX_PARENTS {
            return corrupt("commit graph record has too many parents");
        }
        count
            .checked_add(record.parents.len())
            .ok_or_else(|| MetadataError::Internal("commit graph parent count overflow".to_owned()))
    })?;
    let capacity = LAYER_HEADER_BYTES
        .checked_add(layer.records.len().saturating_mul(RECORD_FIXED_BYTES))
        .and_then(|size| size.checked_add(parent_count.saturating_mul(4)))
        .ok_or_else(|| MetadataError::Internal("commit graph layer size overflow".to_owned()))?;
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(LAYER_MAGIC);
    bytes.extend_from_slice(&LAYER_VERSION.to_le_bytes());
    bytes.extend_from_slice(&layer.base_ordinal.to_le_bytes());
    bytes.extend_from_slice(&(layer.records.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(parent_count as u32).to_le_bytes());
    for record in &layer.records {
        bytes.extend_from_slice(&record.oid);
        bytes.extend_from_slice(&record.tree_oid);
        bytes.extend_from_slice(&record.commit_time.to_le_bytes());
        bytes.extend_from_slice(&record.corrected_generation.to_le_bytes());
        bytes.extend_from_slice(&(record.parents.len() as u32).to_le_bytes());
        for parent in &record.parents {
            bytes.extend_from_slice(&parent.to_le_bytes());
        }
    }
    Ok(bytes)
}

pub fn decode_commit_graph_layer(bytes: &[u8], path: &str) -> Result<CommitGraphLayer> {
    if bytes.len() < LAYER_HEADER_BYTES || &bytes[..8] != LAYER_MAGIC {
        return corrupt_at(path, "invalid commit graph layer header");
    }
    let mut cursor = 8;
    let version = read_u32(bytes, &mut cursor, path)?;
    if version != LAYER_VERSION {
        return corrupt_at(path, "unsupported commit graph layer version");
    }
    let base_ordinal = read_u32(bytes, &mut cursor, path)?;
    let record_count = read_u32(bytes, &mut cursor, path)? as usize;
    let declared_parents = read_u32(bytes, &mut cursor, path)? as usize;
    let minimum = LAYER_HEADER_BYTES
        .checked_add(record_count.saturating_mul(RECORD_FIXED_BYTES))
        .and_then(|size| size.checked_add(declared_parents.saturating_mul(4)))
        .ok_or_else(|| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "commit graph layer size overflow".to_owned(),
        })?;
    if minimum != bytes.len() {
        return corrupt_at(path, "commit graph layer length does not match header");
    }
    let mut records = Vec::with_capacity(record_count);
    let mut observed_parents = 0usize;
    for _ in 0..record_count {
        let oid = read_oid(bytes, &mut cursor, path)?;
        let tree_oid = read_oid(bytes, &mut cursor, path)?;
        let commit_time = read_i64(bytes, &mut cursor, path)?;
        let corrected_generation = read_u64(bytes, &mut cursor, path)?;
        let parent_count = read_u32(bytes, &mut cursor, path)? as usize;
        if parent_count > MAX_PARENTS {
            return corrupt_at(path, "commit graph record has too many parents");
        }
        observed_parents = observed_parents.saturating_add(parent_count);
        let mut parents = Vec::with_capacity(parent_count);
        for _ in 0..parent_count {
            parents.push(read_u32(bytes, &mut cursor, path)?);
        }
        records.push(CommitGraphRecord {
            oid,
            tree_oid,
            commit_time,
            corrected_generation,
            parents,
        });
    }
    if cursor != bytes.len() || observed_parents != declared_parents {
        return corrupt_at(path, "commit graph parent count does not match header");
    }
    Ok(CommitGraphLayer {
        base_ordinal,
        records,
    })
}

/// Load and content-verify a complete split graph.
#[cfg(feature = "storage")]
pub async fn load_split_commit_graph(
    store: &Store,
    router: &StoreLayout<Store>,
    descriptor_hash: &str,
    max_bytes: u64,
) -> Result<SplitCommitGraph> {
    validate_content_hash(
        descriptor_hash,
        "commit graph descriptor hash",
        "commit graph",
    )?;
    let descriptor_path = router.bulk_manifest_path("commit-graph", descriptor_hash);
    let expected = decode_hash(descriptor_hash, descriptor_path.as_ref())?;
    let descriptor_bytes = store.verify(&descriptor_path, &expected).await?;
    let mut fetched_bytes = descriptor_bytes.len() as u64;
    if fetched_bytes > max_bytes {
        return Err(MetadataError::CorruptObject {
            path: descriptor_path.to_string(),
            reason: format!("commit graph exceeds {max_bytes} byte limit"),
        });
    }
    let descriptor = decode_commit_graph_descriptor(&descriptor_bytes, descriptor_path.as_ref())?;
    let mut layers = Vec::with_capacity(descriptor.layers.len());
    for reference in &descriptor.layers {
        fetched_bytes = fetched_bytes.checked_add(reference.bytes).ok_or_else(|| {
            MetadataError::CorruptObject {
                path: descriptor_path.to_string(),
                reason: "commit graph byte count overflow".to_owned(),
            }
        })?;
        if fetched_bytes > max_bytes {
            return Err(MetadataError::CorruptObject {
                path: descriptor_path.to_string(),
                reason: format!("commit graph exceeds {max_bytes} byte limit"),
            });
        }
        let path = router.repo_path(&reference.path);
        let expected = decode_hash(&reference.hash, path.as_ref())?;
        let bytes = store.verify(&path, &expected).await?;
        if bytes.len() as u64 != reference.bytes {
            return Err(MetadataError::CorruptObject {
                path: path.to_string(),
                reason: "commit graph layer byte length mismatch".to_owned(),
            });
        }
        layers.push(decode_commit_graph_layer(&bytes, path.as_ref())?);
    }
    SplitCommitGraph::new(descriptor, layers)
}

/// Upload changed layers before the immutable descriptor object.
#[cfg(feature = "storage")]
pub async fn upload_split_commit_graph(
    store: &Store,
    router: &StoreLayout<Store>,
    write: &CommitGraphWrite,
) -> Result<()> {
    for layer in &write.layers {
        upload_if_absent(
            store,
            &router.repo_path(&layer.reference.path),
            &layer.bytes,
        )
        .await?;
    }
    upload_if_absent(
        store,
        &router.bulk_manifest_path("commit-graph", &write.descriptor_hash),
        &write.descriptor_bytes,
    )
    .await
}

#[cfg(feature = "storage")]
async fn upload_if_absent(
    store: &Store,
    path: &object_store::path::Path,
    bytes: &[u8],
) -> Result<()> {
    match store.head(path).await {
        Ok(_) => Ok(()),
        Err(StorageError::NotFound { .. }) => store
            .put(path, Bytes::copy_from_slice(bytes))
            .await
            .map_err(MetadataError::from),
        Err(error) => Err(MetadataError::from(error)),
    }
}

#[cfg(feature = "storage")]
fn decode_hash(value: &str, path: &str) -> Result<[u8; 32]> {
    blake3::Hash::from_hex(value)
        .map(|hash| *hash.as_bytes())
        .map_err(|error| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: format!("invalid content hash: {error}"),
        })
}

fn validate_descriptor(
    descriptor: &CommitGraphDescriptor,
    layers: &[CommitGraphLayer],
) -> Result<()> {
    validate_descriptor_shape(descriptor, "commit graph descriptor")?;
    if descriptor.layers.len() != layers.len() {
        return corrupt("commit graph descriptor layer count mismatch");
    }
    for (reference, layer) in descriptor.layers.iter().zip(layers) {
        if reference.base_ordinal != layer.base_ordinal
            || reference.commit_count as usize != layer.records.len()
        {
            return corrupt("commit graph descriptor does not match decoded layer");
        }
    }
    Ok(())
}

fn validate_descriptor_shape(descriptor: &CommitGraphDescriptor, path: &str) -> Result<()> {
    if descriptor.version != LAYER_VERSION {
        return corrupt_at(path, "unsupported commit graph descriptor version");
    }
    validate_content_hash(
        &descriptor.pack_index_hash,
        "commit graph pack-index hash",
        path,
    )?;
    validate_content_hash(
        &descriptor.git_validation_digest,
        "commit graph validation digest",
        path,
    )?;
    let mut next = 0u32;
    for layer in &descriptor.layers {
        validate_content_hash(&layer.hash, "commit graph layer hash", path)?;
        if layer.path != commit_graph_layer_path(&layer.hash)
            || layer.base_ordinal != next
            || layer.commit_count == 0
            || layer.bytes < LAYER_HEADER_BYTES as u64
        {
            return corrupt_at(path, "invalid commit graph layer reference");
        }
        next =
            next.checked_add(layer.commit_count)
                .ok_or_else(|| MetadataError::CorruptObject {
                    path: path.to_owned(),
                    reason: "commit graph ordinal overflow".to_owned(),
                })?;
    }
    if next != descriptor.commit_count {
        return corrupt_at(path, "commit graph descriptor commit count mismatch");
    }
    Ok(())
}

fn read_oid(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<[u8; 20]> {
    let end = cursor.saturating_add(20);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "truncated commit graph layer".to_owned(),
        })?;
    *cursor = end;
    let mut oid = [0; 20];
    oid.copy_from_slice(value);
    Ok(oid)
}

fn read_u32(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(bytes, cursor, path)?))
}

fn read_u64(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array(bytes, cursor, path)?))
}

fn read_i64(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<i64> {
    Ok(i64::from_le_bytes(read_array(bytes, cursor, path)?))
}

fn read_array<const N: usize>(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<[u8; N]> {
    let end = cursor.saturating_add(N);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "truncated commit graph layer".to_owned(),
        })?;
    *cursor = end;
    let mut out = [0; N];
    out.copy_from_slice(value);
    Ok(out)
}

fn non_negative_time(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn sha1_hex(oid: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(40);
    for byte in oid {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn parse_sha1_hex(value: &str) -> Option<[u8; 20]> {
    if value.len() != 40 {
        return None;
    }
    let mut oid = [0_u8; 20];
    for (index, byte) in oid.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&value[offset..offset + 2], 16).ok()?;
    }
    Some(oid)
}

fn corrupt<T>(reason: &str) -> Result<T> {
    corrupt_at("commit graph", reason)
}

fn corrupt_at<T>(path: &str, reason: &str) -> Result<T> {
    Err(MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.to_owned(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn oid(value: u8) -> [u8; 20] {
        [value; 20]
    }

    fn input(value: u8, time: i64, parents: &[u8]) -> CommitGraphInput {
        CommitGraphInput {
            oid: oid(value),
            tree_oid: oid(value.saturating_add(100)),
            commit_time: time,
            parents: parents.iter().copied().map(oid).collect(),
        }
    }

    fn hash(value: u8) -> String {
        format!("{value:02x}").repeat(32)
    }

    fn append(
        base: Option<SplitCommitGraph>,
        generation: u64,
        roots: &[[u8; 20]],
        additions: Vec<CommitGraphInput>,
    ) -> (CommitGraphWrite, SplitCommitGraph) {
        let write = append_split_commit_graph(base, generation, hash(1), hash(2), roots, additions)
            .unwrap()
            .unwrap();
        let descriptor =
            decode_commit_graph_descriptor(&write.descriptor_bytes, "descriptor").unwrap();
        let layers = descriptor
            .layers
            .iter()
            .map(|reference| {
                let object = write
                    .layers
                    .iter()
                    .find(|object| object.reference.hash == reference.hash)
                    .unwrap();
                decode_commit_graph_layer(&object.bytes, &reference.path).unwrap()
            })
            .collect();
        let graph = SplitCommitGraph::new(descriptor, layers).unwrap();
        (write, graph)
    }

    #[test]
    fn binary_layer_round_trip_preserves_merge_parent_order() {
        let layer = CommitGraphLayer {
            base_ordinal: 2,
            records: vec![CommitGraphRecord {
                oid: oid(3),
                tree_oid: oid(103),
                commit_time: 30,
                corrected_generation: 30,
                parents: vec![1, 0],
            }],
        };
        let bytes = encode_commit_graph_layer(&layer).unwrap();
        assert_eq!(decode_commit_graph_layer(&bytes, "layer").unwrap(), layer);
    }

    #[test]
    fn append_orders_parents_before_children_and_answers_ancestry() {
        let (_, graph) = append(
            None,
            1,
            &[oid(4)],
            vec![
                input(4, 40, &[3]),
                input(2, 20, &[1]),
                input(1, 10, &[]),
                input(3, 30, &[2]),
            ],
        );
        assert_eq!(graph.descriptor.commit_count, 4);
        assert_eq!(graph.is_ancestor(&oid(1), &oid(4)), Some(true));
        assert_eq!(graph.is_ancestor(&oid(4), &oid(1)), Some(false));
        assert_eq!(graph.shallow_boundary(&[oid(4)], 2), Some(vec![oid(3)]));
        let tips = vec![sha1_hex(&oid(4))];
        let boundary =
            crate::commit_graph::CommitGraphTraversal::shallow_boundary(&graph, &tips, 2).unwrap();
        assert_eq!(boundary, [sha1_hex(&oid(3))]);
        assert_eq!(
            crate::commit_graph::CommitGraphTraversal::is_reachable_from(
                &graph,
                &tips,
                &sha1_hex(&oid(1)),
            ),
            Some(true)
        );
    }

    #[test]
    fn append_returns_none_when_parent_closure_is_incomplete() {
        let result = append_split_commit_graph(
            None,
            1,
            hash(1),
            hash(2),
            &[oid(2)],
            vec![input(2, 20, &[1])],
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn geometric_compaction_keeps_layer_count_logarithmic() {
        let mut graph = None;
        let mut objects = HashMap::<String, Vec<u8>>::new();
        for value in 1..=64u8 {
            let additions = vec![input(
                value,
                i64::from(value),
                value.checked_sub(1).filter(|p| *p > 0).as_slice(),
            )];
            let write = append_split_commit_graph(
                graph,
                u64::from(value),
                hash(1),
                hash(2),
                &[oid(value)],
                additions,
            )
            .unwrap()
            .unwrap();
            for object in &write.layers {
                objects.insert(object.reference.hash.clone(), object.bytes.clone());
            }
            let descriptor =
                decode_commit_graph_descriptor(&write.descriptor_bytes, "descriptor").unwrap();
            let layers = descriptor
                .layers
                .iter()
                .map(|reference| {
                    decode_commit_graph_layer(&objects[&reference.hash], &reference.path).unwrap()
                })
                .collect();
            let appended = SplitCommitGraph::new(descriptor, layers).unwrap();
            assert_eq!(write.layers.len(), 1, "push rewrites only its tip layer");
            graph = if let Some(compacted) = compact_split_commit_graph(appended.clone()).unwrap() {
                for object in &compacted.layers {
                    objects.insert(object.reference.hash.clone(), object.bytes.clone());
                }
                let descriptor =
                    decode_commit_graph_descriptor(&compacted.descriptor_bytes, "descriptor")
                        .unwrap();
                let layers = descriptor
                    .layers
                    .iter()
                    .map(|reference| {
                        decode_commit_graph_layer(&objects[&reference.hash], &reference.path)
                            .unwrap()
                    })
                    .collect();
                Some(SplitCommitGraph::new(descriptor, layers).unwrap())
            } else {
                Some(appended)
            };
        }
        let graph = graph.unwrap();
        assert_eq!(graph.descriptor.commit_count, 64);
        assert_eq!(graph.layers.len(), 1);
        assert_eq!(graph.is_ancestor(&oid(1), &oid(64)), Some(true));
    }

    #[test]
    fn rejects_parent_that_is_not_earlier_than_child() {
        let descriptor = CommitGraphDescriptor {
            version: 1,
            generation: 1,
            pack_index_hash: hash(1),
            git_validation_digest: hash(2),
            commit_count: 1,
            layers: vec![CommitGraphLayerRef {
                hash: hash(3),
                path: commit_graph_layer_path(&hash(3)),
                base_ordinal: 0,
                commit_count: 1,
                bytes: 88,
            }],
        };
        let layer = CommitGraphLayer {
            base_ordinal: 0,
            records: vec![CommitGraphRecord {
                oid: oid(1),
                tree_oid: oid(101),
                commit_time: 1,
                corrected_generation: 1,
                parents: vec![0],
            }],
        };
        assert!(SplitCommitGraph::new(descriptor, vec![layer]).is_err());
    }
}
