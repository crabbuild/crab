//! Exact shared physical-object closures for multi-ref publication preparation.
//! Blob payload integrity remains the responsibility of pack admission.
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use gix_hash::ObjectId;
use gix_object::{Find, FindHeader, Kind};

/// One chunk represents 1,024 consecutive sorted dictionary objects.
pub const CHUNK_BYTES: usize = 128;
const CHUNK_BITS: usize = CHUNK_BYTES * 8;

/// Independent work and allocation bounds for one shared graph preparation.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_refs: usize,
    pub max_objects: usize,
    /// Parsed links, including repeated links, bound decoding and propagation work.
    pub max_links: usize,
    pub max_object_bytes: usize,
    pub max_structural_bytes: u64,
    pub max_matrix_bytes: usize,
    /// Conservative allocation estimates, not the encoded visibility wire limit.
    pub max_memory_bytes: usize,
    pub max_output_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_refs: 100_000,
            max_objects: 10_000_000,
            max_links: 256_000_000,
            max_object_bytes: 64 * 1024 * 1024,
            max_structural_bytes: 16 * 1024 * 1024 * 1024,
            max_matrix_bytes: 1024 * 1024 * 1024,
            max_memory_bytes: 3 * 1024 * 1024 * 1024,
            max_output_bytes: 384 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("shared Git visibility preparation cancelled")]
    Cancelled,
    #[error("shared Git visibility exceeds {kind} budget ({maximum})")]
    Limit { kind: &'static str, maximum: u64 },
    #[error("shared Git visibility is missing object {0}")]
    Missing(ObjectId),
    #[error("invalid shared Git visibility object {oid}: {reason}")]
    Invalid { oid: ObjectId, reason: &'static str },
    #[error("shared Git object graph contains a cycle")]
    Cycle,
    #[error("shared Git visibility object lookup or parsing failed")]
    Source(#[source] Box<dyn std::error::Error + Send + Sync>),
}
type Result<T> = std::result::Result<T, Error>;
fn source(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
    Error::Source(error.into())
}
fn bound(value: usize, maximum: usize, kind: &'static str) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            kind,
            maximum: maximum as u64,
        });
    }
    Ok(())
}
fn product(a: usize, b: usize) -> Result<usize> {
    a.checked_mul(b).ok_or(Error::Limit {
        kind: "allocation arithmetic",
        maximum: usize::MAX as u64,
    })
}
fn cancelled(check: &impl Fn() -> bool) -> Result<()> {
    if check() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// One exact set over the result's sorted dictionary, retaining shared chunk ownership.
#[derive(Debug, Clone)]
pub struct Closure {
    chunks: Vec<Arc<[u8; CHUNK_BYTES]>>,
    count: usize,
}
impl Closure {
    pub fn chunks(&self) -> &[Arc<[u8; CHUNK_BYTES]>] {
        &self.chunks
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Exact closures in input-root order; dictionary presence alone grants no membership.
#[derive(Debug)]
pub struct SharedVisibility {
    objects: Vec<ObjectId>,
    closures: Vec<Closure>,
    pub structural_bytes: u64,
    pub parsed_links: usize,
    pub distinct_links: usize,
}
impl SharedVisibility {
    pub fn objects(&self) -> &[ObjectId] {
        &self.objects
    }
    pub fn closures(&self) -> &[Closure] {
        &self.closures
    }
}
struct Node {
    oid: ObjectId,
    kind: Option<Kind>,
    links: Box<[u32]>,
}
struct Graph {
    nodes: Vec<Node>,
    positions: HashMap<ObjectId, u32>,
    parsed_links: usize,
    distinct_links: usize,
    structural_bytes: u64,
}
impl Graph {
    fn memory(&self, limits: Limits, extra: usize) -> Result<()> {
        // Include growth slack for nodes/hash buckets, link buffers, traversal
        // arrays and a structural object's decoding buffer before allocating.
        let buffers = product(limits.max_object_bytes, 2)?;
        let bytes = product(self.nodes.len(), 256)?
            .checked_add(product(self.parsed_links, 8)?)
            .and_then(|bytes| bytes.checked_add(buffers))
            .and_then(|bytes| bytes.checked_add(extra))
            .ok_or(Error::Limit {
                kind: "allocation arithmetic",
                maximum: usize::MAX as u64,
            })?;
        bound(bytes, limits.max_memory_bytes, "memory")
    }
    fn admit(&mut self, oid: ObjectId, kind: Option<Kind>, limits: Limits) -> Result<u32> {
        if let Some(&position) = self.positions.get(&oid) {
            let node = &mut self.nodes[position as usize];
            if node.kind.is_some() && kind.is_some() && node.kind != kind {
                return Err(Error::Invalid {
                    oid,
                    reason: "inconsistent referenced kind",
                });
            }
            node.kind = node.kind.or(kind);
            return Ok(position);
        }
        bound(
            self.nodes.len().saturating_add(1),
            limits.max_objects.min(u32::MAX as usize),
            "distinct objects",
        )?;
        // Admission must pay for the next node before Vec/HashMap growth.
        self.memory(limits, 256)?;
        let position = self.nodes.len() as u32;
        self.positions.insert(oid, position);
        self.nodes.push(Node {
            oid,
            kind,
            links: Box::new([]),
        });
        Ok(position)
    }
    fn link(
        &mut self,
        oid: ObjectId,
        kind: Kind,
        links: &mut Vec<u32>,
        limits: Limits,
        check: &impl Fn() -> bool,
    ) -> Result<()> {
        self.parsed_links = self.parsed_links.saturating_add(1);
        bound(self.parsed_links, limits.max_links, "structural links")?;
        if self.parsed_links.is_multiple_of(4096) {
            cancelled(check)?;
        }
        self.memory(limits, 0)?;
        links.push(self.admit(oid, Some(kind), limits)?);
        Ok(())
    }
}

/// Visit physical Git objects once and propagate exact memberships to every root.
///
/// Replacements, grafts and shallow boundaries cannot change physical ancestry.
/// Missing parents fail closed. Structural data is checksum-verified; tree gitlinks
/// do not traverse another repository. Pack admission must still verify blob bytes.
pub fn build(
    git_dir: &Path,
    roots: &[ObjectId],
    limits: Limits,
    check: impl Fn() -> bool,
) -> Result<SharedVisibility> {
    cancelled(&check)?;
    bound(roots.len(), limits.max_refs, "refs")?;
    bound(
        product(roots.len(), 64)?
            .checked_add(product(limits.max_object_bytes, 2)?)
            .ok_or(Error::Limit {
                kind: "allocation arithmetic",
                maximum: usize::MAX as u64,
            })?,
        limits.max_memory_bytes,
        "memory",
    )?;
    let odb = gix_odb::at_opts(
        git_dir.join("objects"),
        [],
        gix_odb::store::init::Options {
            alloc_limit_bytes: Some(limits.max_object_bytes),
            ..Default::default()
        },
    )
    .map_err(source)?;
    let mut graph = Graph {
        nodes: Vec::new(),
        positions: HashMap::new(),
        parsed_links: 0,
        distinct_links: 0,
        structural_bytes: 0,
    };
    let mut starts = Vec::with_capacity(roots.len());
    for &oid in roots {
        cancelled(&check)?;
        if oid.kind() != gix_hash::Kind::Sha1 {
            return Err(Error::Invalid {
                oid,
                reason: "only SHA-1 object identities are supported",
            });
        }
        starts.push(graph.admit(oid, None, limits)? as usize);
    }
    let mut buffer = Vec::new();
    let mut index = 0;
    while index < graph.nodes.len() {
        cancelled(&check)?;
        let oid = graph.nodes[index].oid;
        let expected = graph.nodes[index].kind;
        let remaining = limits
            .max_structural_bytes
            .saturating_sub(graph.structural_bytes);
        let kind = match expected {
            Some(kind) if kind != Kind::Blob && remaining >= limits.max_object_bytes as u64 => kind,
            _ => {
                let header = odb
                    .try_header(&oid)
                    .map_err(source)?
                    .ok_or(Error::Missing(oid))?;
                if expected.is_some_and(|kind| kind != header.kind) {
                    return Err(Error::Invalid {
                        oid,
                        reason: "referenced object has wrong kind",
                    });
                }
                if header.kind != Kind::Blob {
                    bound(
                        usize::try_from(header.size).unwrap_or(usize::MAX),
                        limits.max_object_bytes,
                        "structural object bytes",
                    )?;
                    if header.size > remaining {
                        return Err(Error::Limit {
                            kind: "structural read bytes",
                            maximum: limits.max_structural_bytes,
                        });
                    }
                }
                header.kind
            }
        };
        graph.nodes[index].kind = Some(kind);
        let mut links = Vec::new();
        if kind != Kind::Blob {
            let object = odb
                .try_find(&oid, &mut buffer)
                .map_err(source)?
                .ok_or(Error::Missing(oid))?;
            if object.kind != kind {
                return Err(Error::Invalid {
                    oid,
                    reason: "referenced object has wrong kind",
                });
            }
            bound(
                object.data.len(),
                limits.max_object_bytes,
                "structural object bytes",
            )?;
            object.verify_checksum(&oid).map_err(source)?;
            graph.structural_bytes = graph
                .structural_bytes
                .saturating_add(object.data.len() as u64);
            if graph.structural_bytes > limits.max_structural_bytes {
                return Err(Error::Limit {
                    kind: "structural read bytes",
                    maximum: limits.max_structural_bytes,
                });
            }
            match kind {
                Kind::Commit => {
                    let commit =
                        gix_object::CommitRef::from_bytes(object.data, gix_hash::Kind::Sha1)
                            .map_err(source)?;
                    graph.link(commit.tree(), Kind::Tree, &mut links, limits, &check)?;
                    for parent in commit.parents() {
                        graph.link(parent, Kind::Commit, &mut links, limits, &check)?;
                    }
                }
                Kind::Tag => {
                    let tag = gix_object::TagRef::from_bytes(object.data, gix_hash::Kind::Sha1)
                        .map_err(source)?;
                    graph.link(tag.target(), tag.target_kind, &mut links, limits, &check)?;
                }
                Kind::Tree => {
                    for entry in
                        gix_object::TreeRefIter::from_bytes(object.data, gix_hash::Kind::Sha1)
                    {
                        let entry = entry.map_err(source)?;
                        let kind = match entry.mode.kind() {
                            gix_object::tree::EntryKind::Tree => Some(Kind::Tree),
                            gix_object::tree::EntryKind::Blob
                            | gix_object::tree::EntryKind::BlobExecutable
                            | gix_object::tree::EntryKind::Link => Some(Kind::Blob),
                            gix_object::tree::EntryKind::Commit => None,
                        };
                        if let Some(kind) = kind {
                            graph.link(entry.oid.to_owned(), kind, &mut links, limits, &check)?;
                        }
                    }
                }
                Kind::Blob => unreachable!(),
            }
        }
        links.sort_unstable();
        links.dedup();
        graph.distinct_links += links.len();
        graph.nodes[index].links = links.into_boxed_slice();
        index += 1;
    }
    // The map is no longer needed once all raw objects have been admitted.
    graph.positions.clear();
    graph.positions.shrink_to_fit();
    let words = roots.len().div_ceil(64);
    let matrix_bytes = product(product(graph.nodes.len(), words)?, 8)?;
    bound(matrix_bytes, limits.max_matrix_bytes, "membership matrix")?;
    graph.memory(limits, matrix_bytes)?;
    let mut membership = vec![0u64; matrix_bytes / 8];
    for (bit, &start) in starts.iter().enumerate() {
        membership[start * words + bit / 64] |= 1 << (bit % 64);
    }
    propagate(&graph.nodes, words, &mut membership, &check)?;
    // Retire adjacency before allocating the transposed chunk pool.
    for node in &mut graph.nodes {
        node.links = Box::new([]);
    }
    let mut sorted = (0..graph.nodes.len()).collect::<Vec<_>>();
    sorted.sort_unstable_by_key(|&index| graph.nodes[index].oid);
    let slots = sorted.len().div_ceil(CHUNK_BITS);
    let fixed_output = product(
        product(roots.len(), slots)?,
        std::mem::size_of::<Arc<[u8; CHUNK_BYTES]>>(),
    )?
    .checked_add(product(sorted.len(), 32)?)
    .ok_or(Error::Limit {
        kind: "output arithmetic",
        maximum: usize::MAX as u64,
    })?;
    let fixed_output = fixed_output
        .checked_add(product(roots.len(), 64)?)
        .ok_or(Error::Limit {
            kind: "output arithmetic",
            maximum: usize::MAX as u64,
        })?;
    bound(fixed_output, limits.max_output_bytes, "output")?;
    let block_bytes = product(roots.len(), CHUNK_BYTES)?;
    let fixed_working = product(graph.nodes.len(), 256)?
        .checked_add(matrix_bytes)
        .and_then(|n| n.checked_add(fixed_output))
        .and_then(|n| n.checked_add(block_bytes))
        .ok_or(Error::Limit {
            kind: "allocation arithmetic",
            maximum: usize::MAX as u64,
        })?;
    bound(fixed_working, limits.max_memory_bytes, "memory")?;
    let mut closures = (0..roots.len())
        .map(|_| Closure {
            chunks: Vec::with_capacity(slots),
            count: 0,
        })
        .collect::<Vec<_>>();
    let mut pool = HashMap::<[u8; CHUNK_BYTES], Arc<[u8; CHUNK_BYTES]>>::new();
    let mut block = vec![[0u8; CHUNK_BYTES]; roots.len()];
    for group in sorted.chunks(CHUNK_BITS) {
        cancelled(&check)?;
        block.fill([0; CHUNK_BYTES]);
        for (position, &node) in group.iter().enumerate() {
            if position.is_multiple_of(128) {
                cancelled(&check)?;
            }
            for word in 0..words {
                let mut bits = membership[node * words + word];
                while bits != 0 {
                    let reference = word * 64 + bits.trailing_zeros() as usize;
                    block[reference][position / 8] |= 1 << (position % 8);
                    bits &= bits - 1;
                }
            }
        }
        for (closure, chunk) in closures.iter_mut().zip(&block) {
            let shared = if let Some(shared) = pool.get(chunk) {
                shared.clone()
            } else {
                // Pay for hash growth slack, key bytes, owned Arc chunks and headers.
                let output = fixed_output
                    .checked_add(product(pool.len().saturating_add(1), 512)?)
                    .ok_or(Error::Limit {
                        kind: "output arithmetic",
                        maximum: usize::MAX as u64,
                    })?;
                bound(output, limits.max_output_bytes, "output")?;
                let working = product(graph.nodes.len(), 256)?
                    .checked_add(matrix_bytes)
                    .and_then(|n| n.checked_add(output))
                    .and_then(|n| n.checked_add(roots.len().saturating_mul(CHUNK_BYTES)))
                    .ok_or(Error::Limit {
                        kind: "allocation arithmetic",
                        maximum: usize::MAX as u64,
                    })?;
                bound(working, limits.max_memory_bytes, "memory")?;
                let shared = Arc::new(*chunk);
                pool.insert(*chunk, shared.clone());
                shared
            };
            closure.count += chunk
                .iter()
                .map(|byte| byte.count_ones() as usize)
                .sum::<usize>();
            closure.chunks.push(shared);
        }
    }
    cancelled(&check)?;
    Ok(SharedVisibility {
        objects: sorted
            .into_iter()
            .map(|index| graph.nodes[index].oid)
            .collect(),
        closures,
        structural_bytes: graph.structural_bytes,
        parsed_links: graph.parsed_links,
        distinct_links: graph.distinct_links,
    })
}
fn propagate(
    nodes: &[Node],
    words: usize,
    membership: &mut [u64],
    check: &impl Fn() -> bool,
) -> Result<()> {
    let mut incoming = vec![0usize; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        if index.is_multiple_of(1024) {
            cancelled(check)?;
        }
        for &child in &node.links {
            incoming[child as usize] += 1;
        }
    }
    let mut ready = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut visited = 0;
    while let Some(node) = ready.pop_front() {
        if visited % 1024 == 0 {
            cancelled(check)?;
        }
        for (link, &child) in nodes[node].links.iter().enumerate() {
            if link.is_multiple_of(4096) {
                cancelled(check)?;
            }
            let child = child as usize;
            for word in 0..words {
                membership[child * words + word] |= membership[node * words + word];
            }
            incoming[child] -= 1;
            if incoming[child] == 0 {
                ready.push_back(child);
            }
        }
        visited += 1;
    }
    if visited != nodes.len() {
        return Err(Error::Cycle);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
