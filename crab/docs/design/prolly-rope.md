Below is a long-term design for a native Prolly Rope: a persistent, content-addressed, sequence-oriented tree for large-file chunk recipes.

The key idea:

ProllyMap<K, V>
  = sorted key-value tree
ProllyRope<T>
  = ordered sequence tree

For your large-file storage system:

type LargeFileRecipe = ProllyRope<LogicalChunkRef>;

where each item is:

struct LogicalChunkRef {
    chunk_hash: Hash,
    raw_len: u32,
}

The rope stores the logical chunk recipe, while xet-style xorbs/CAS store the actual bytes.

⸻

1. Why Prolly Rope exists

You already have a prolly tree with a map interface:

get(key)
put(key, value)
delete(key)
diff(left, right)
merge(base, left, right)

That is excellent for:

path -> file metadata
branch name -> reference
table primary key -> row
object id -> object metadata

But a large file recipe is not naturally a map.

A file is a sequence:

chunk_0, chunk_1, chunk_2, chunk_3, ...

For example:

A B C D E F G H

The important property is order, not lookup by explicit key.

If you force this into a map, you need fake keys:

0       -> A
65536   -> B
131072  -> C
196608  -> D

or:

0 -> A
1 -> B
2 -> C
3 -> D

That works for simple lookup, but it is bad for versioned files.

Suppose you insert a new chunk after B:

Before:
A B C D E F
After:
A B X C D E F

With offset keys, everything after B shifts:

Before:
0  -> A
10 -> B
20 -> C
30 -> D
40 -> E
50 -> F
After:
0  -> A
10 -> B
20 -> X
30 -> C
40 -> D
50 -> E
60 -> F

C, D, E, and F did not change, but their keys changed. That causes unnecessary metadata churn.

A rope avoids this.

It represents order by:

child order + leaf entry order

not by explicit per-chunk keys.

So the file:

A B C D E F

can be stored as:

root
 ├── leaf_1: A B
 ├── leaf_2: C D
 └── leaf_3: E F

After inserting X:

root'
 ├── leaf_1: A B      reused
 ├── leaf_4: X        new
 ├── leaf_2: C D      reused
 └── leaf_3: E F      reused

No downstream keys changed.

That is the central reason to use a native Prolly Rope.

⸻

2. What the rope stores

The rope should not store raw bytes.

For xet-style storage, split the system into three layers:

Layer 1: Logical recipe
  ProllyRope<LogicalChunkRef>
  stores ordered chunk hashes and lengths
Layer 2: Physical placement index
  chunk_hash -> xorb_id + chunk_index + offsets
Layer 3: Byte storage
  xorbs / packs / CAS objects in S3 or local store

So a large file version looks like this:

LargeFileVersion
  file_id
  version_id
  file_size
  file_hash
  recipe_root_block_id

The recipe root points to a Prolly Rope:

recipe_root
  -> internal nodes
      -> leaf nodes
          -> LogicalChunkRef[]

Each LogicalChunkRef is:

pub struct LogicalChunkRef {
    pub chunk_hash: Hash,
    pub raw_len: u32,
}

The physical xorb location is separate:

pub struct ChunkLocation {
    pub xorb_id: XorbId,
    pub chunk_index: u32,
    pub raw_len: u32,
    pub compressed_offset: Option<u64>,
    pub compressed_len: Option<u32>,
}

This separation matters.

The rope root should represent:

ordered logical content

not:

current physical storage layout

That way, repacking xorbs does not change file version identity.

⸻

3. Mental model

A Prolly Rope is a persistent Merkle tree over a sequence.

For a tiny file:

A B C D E F G H

A rope could look like:

                          root
             total_bytes = len(A..H)
             total_items = 8
             digest = H(...)
          ┌──────────────┼──────────────┐
          v              v              v
       leaf_1          leaf_2          leaf_3
       A B C           D E             F G H

For a real 10GB file:

10 GiB / 64 KiB ≈ 163,840 chunks

If each leaf has around 512 chunk refs:

163,840 / 512 ≈ 320 leaves

With internal fanout around 128:

level 0: ~320 leaves
level 1: ~3 internal nodes
level 2: 1 root

So metadata is small.

The raw 10GB is not inside the tree. The tree only stores the reconstruction recipe.

⸻

4. Core design principles

Principle 1: sequence order is implicit

There is no persistent chunk key like:

byte_offset
chunk_index
chunk_hash
xorb_offset

The order is:

root.children[0], root.children[1], ...
leaf.entries[0], leaf.entries[1], ...

The full sequence of a node is the concatenation of its children.

node.sequence = child_0.sequence || child_1.sequence || child_2.sequence

Principle 2: range lookup uses prefix sums

Every node stores:

total_bytes
total_items

To find byte offset 5 GiB, you descend by subtracting child sizes.

Example:

root:
  child_0 total_bytes = 3 GiB
  child_1 total_bytes = 3 GiB
  child_2 total_bytes = 4 GiB

Offset 5 GiB:

skip child_0:
  offset = 5 - 3 = 2 GiB
descend into child_1 with local offset 2 GiB

Principle 3: block identity is content-addressed

Every node is encoded canonically.

block_id = hash(canonical_node_bytes)

If two versions produce the same subtree, they automatically share the same block.

Principle 4: boundaries are content-derived

Like a prolly tree, block boundaries are chosen using content-derived hashes.

For a rope leaf, the boundary input is:

chunk_hash || raw_len

Not xorb location.

For parent nodes, the boundary input is:

child_digest || child_total_bytes || child_total_items

This gives stable boundaries across versions when content is mostly unchanged.

Principle 5: write path is copy-on-write

Every mutation returns a new root.

Old versions remain valid.

root_v1 -> old tree
root_v2 -> new tree sharing most old blocks

No in-place mutation.

⸻

5. Formal data model

5.1 Basic IDs

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Hash256(pub [u8; 32]);
pub type BlockId = Hash256;
pub type ContentHash = Hash256;
pub type XorbId = Hash256;

5.2 Rope item trait

The rope should be generic, but for your first implementation it can target chunk refs only.

Generic trait:

pub trait RopeItem: Clone + Send + Sync + 'static {
    fn logical_len_bytes(&self) -> u64;
    fn stable_digest_bytes(&self, out: &mut Vec<u8>);
    fn boundary_bytes(&self, out: &mut Vec<u8>);
}

Implementation for chunk refs:

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalChunkRef {
    pub chunk_hash: ContentHash,
    pub raw_len: u32,
}
impl RopeItem for LogicalChunkRef {
    fn logical_len_bytes(&self) -> u64 {
        self.raw_len as u64
    }
    fn stable_digest_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.chunk_hash.0);
        out.extend_from_slice(&self.raw_len.to_be_bytes());
    }
    fn boundary_bytes(&self, out: &mut Vec<u8>) {
        self.stable_digest_bytes(out);
    }
}

5.3 Node format

#[derive(Clone, Debug)]
pub enum RopeNode<T> {
    Internal(RopeInternal),
    Leaf(RopeLeaf<T>),
}
#[derive(Clone, Debug)]
pub struct RopeInternal {
    pub level: u8,
    pub total_bytes: u64,
    pub total_items: u64,
    pub digest: Hash256,
    pub children: Vec<RopeChild>,
}
#[derive(Clone, Debug)]
pub struct RopeChild {
    pub block_id: BlockId,
    pub digest: Hash256,
    pub total_bytes: u64,
    pub total_items: u64,
}
#[derive(Clone, Debug)]
pub struct RopeLeaf<T> {
    pub level: u8, // always 0
    pub total_bytes: u64,
    pub total_items: u32,
    pub digest: Hash256,
    pub entries: Vec<T>,
}

The sequence represented by a leaf is:

entries[0] || entries[1] || ... || entries[n - 1]

The sequence represented by an internal node is:

child[0].sequence || child[1].sequence || ... || child[n - 1].sequence

⸻

6. Node invariants

Every valid rope node must satisfy these invariants.

Leaf invariants

For a leaf:

RopeLeaf<T> {
    entries,
    total_bytes,
    total_items,
    digest,
}

Must satisfy:

total_items == entries.len()
total_bytes == sum(entry.logical_len_bytes())
digest == H("leaf", level, total_bytes, total_items, entries...)

The leaf should normally satisfy policy limits:

min_items_per_leaf <= entries.len() <= max_items_per_leaf
encoded_size <= max_leaf_encoded_bytes

Except root may be smaller.

Internal invariants

For an internal node:

RopeInternal {
    children,
    total_bytes,
    total_items,
    digest,
}

Must satisfy:

total_items == sum(child.total_items)
total_bytes == sum(child.total_bytes)
digest == H("internal", level, total_bytes, total_items, children...)

Each child must point to a node with:

child_node.level == parent.level - 1

The child order defines the sequence order.

Content-addressing invariant

For every node:

block_id == H(canonical_node_encoding)

Version identity invariant

A file version root represents only logical chunk content:

ordered sequence of (chunk_hash, raw_len)

It must not include:

xorb_id
chunk_index
physical offset
compression details

⸻

7. Boundary policy

You need separate policies for data chunks and recipe nodes.

Data chunking policy

This is for raw file bytes:

algorithm: FastCDC / Gearhash / Rabin
avg_chunk_size: 64 KiB
min_chunk_size: 8 KiB
max_chunk_size: 128 KiB

Rope metadata boundary policy

This is for recipe tree nodes:

#[derive(Clone, Debug)]
pub struct RopePolicy {
    pub min_leaf_items: usize,
    pub target_leaf_items: usize,
    pub max_leaf_items: usize,
    pub max_leaf_encoded_bytes: usize,
    pub max_leaf_logical_bytes: u64,
    pub min_internal_children: usize,
    pub target_internal_children: usize,
    pub max_internal_children: usize,
    pub leaf_boundary_mask: u64,
    pub internal_boundary_mask: u64,
    pub max_rewrite_window_items: usize,
}

Example starting values:

impl Default for RopePolicy {
    fn default() -> Self {
        Self {
            min_leaf_items: 128,
            target_leaf_items: 512,
            max_leaf_items: 1024,
            max_leaf_encoded_bytes: 64 * 1024,
            max_leaf_logical_bytes: 64 * 1024 * 1024,
            min_internal_children: 64,
            target_internal_children: 256,
            max_internal_children: 512,
            leaf_boundary_mask: (1 << 9) - 1,      // roughly target 512
            internal_boundary_mask: (1 << 8) - 1,  // roughly target 256
            max_rewrite_window_items: 4096,
        }
    }
}

Boundary rule:

after min size is reached:
  split if rolling_hash matches boundary mask
  or max size is reached

Simplified:

fn should_split(
    rolling: u64,
    count: usize,
    encoded_bytes: usize,
    logical_bytes: u64,
    policy: &RopePolicy,
) -> bool {
    if count < policy.min_leaf_items {
        return false;
    }
    if count >= policy.max_leaf_items {
        return true;
    }
    if encoded_bytes >= policy.max_leaf_encoded_bytes {
        return true;
    }
    if logical_bytes >= policy.max_leaf_logical_bytes {
        return true;
    }
    (rolling & policy.leaf_boundary_mask) == 0
}

⸻

8. Canonical hashing

You need two hashes:

digest:
  semantic digest over logical content
block_id:
  hash of full canonical encoded node

In many systems they can be the same. I prefer keeping the concept separate.

Item digest

For LogicalChunkRef:

item_digest = H("chunk-ref", chunk_hash, raw_len)

Leaf digest

leaf_digest = H(
  "rope-leaf-digest-v1",
  total_bytes,
  total_items,
  item_digest_0,
  item_digest_1,
  ...
)

Internal digest

internal_digest = H(
  "rope-internal-digest-v1",
  level,
  total_bytes,
  total_items,
  child_digest_0,
  child_digest_1,
  ...
)

Block ID

block_id = H(
  "rope-block-v1",
  canonical_node_encoding
)

For logical identity and diff skipping, compare block_id or digest.

For storage dedup, use block_id.

⸻

9. BlockStore interface

Reuse the same storage backend as your map engine.

#[async_trait::async_trait]
pub trait BlockStore: Send + Sync {
    async fn get(&self, id: &BlockId) -> Result<Option<bytes::Bytes>>;
    async fn put(&self, bytes: bytes::Bytes) -> Result<BlockId>;
    async fn contains(&self, id: &BlockId) -> Result<bool>;
}

Typed node store:

pub struct RopeStore<S> {
    block_store: S,
}
impl<S: BlockStore> RopeStore<S> {
    pub async fn get_node<T: RopeItem + Decode>(
        &self,
        id: &BlockId,
    ) -> Result<RopeNode<T>> {
        let bytes = self
            .block_store
            .get(id)
            .await?
            .ok_or_else(|| Error::MissingBlock(*id))?;
        decode_rope_node(&bytes)
    }
    pub async fn put_node<T: RopeItem + Encode>(
        &self,
        node: &RopeNode<T>,
    ) -> Result<BlockId> {
        let bytes = encode_rope_node_canonical(node)?;
        self.block_store.put(bytes).await
    }
}

⸻

10. Core API

A long-term native API:

pub struct ProllyRope<S, T> {
    store: RopeStore<S>,
    policy: RopePolicy,
    _marker: std::marker::PhantomData<T>,
}
impl<S, T> ProllyRope<S, T>
where
    S: BlockStore,
    T: RopeItem + Encode + Decode,
{
    pub async fn build_from_items<I>(&self, items: I) -> Result<BlockId>
    where
        I: IntoIterator<Item = T>;
    pub async fn get_at_byte(
        &self,
        root: BlockId,
        offset: u64,
    ) -> Result<Option<RopeCursor<T>>>;
    pub async fn get_at_index(
        &self,
        root: BlockId,
        index: u64,
    ) -> Result<Option<RopeCursor<T>>>;
    pub async fn slice_by_bytes(
        &self,
        root: BlockId,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<T>>;
    pub async fn replace_range(
        &self,
        root: BlockId,
        range: std::ops::Range<u64>,
        replacement: Vec<T>,
    ) -> Result<BlockId>;
    pub async fn diff(
        &self,
        base: BlockId,
        target: BlockId,
    ) -> Result<Vec<RopeDiffSpan>>;
    pub async fn merge(
        &self,
        base: BlockId,
        left: BlockId,
        right: BlockId,
    ) -> Result<RopeMergeResult>;
}

Cursor:

pub struct RopeCursor<T> {
    pub item: T,
    pub item_index: u64,
    pub item_start_byte: u64,
    pub offset_inside_item: u64,
}

⸻

11. Algorithm 1: bulk build

Input:

ordered items:
A B C D E F G H

Output:

root block id

High-level:

1. Build leaf level from ordered items.
2. Build internal parent levels from child refs.
3. Repeat until one root remains.

11.1 Build leaves

Formal algorithm:

BUILD_LEAF_LEVEL(items):
  current = []
  rolling = new rolling hash
  current_bytes = 0
  for item in items:
    append item to current
    rolling.push(item.boundary_bytes)
    current_bytes += item.logical_len
    if SHOULD_SPLIT_LEAF(current, rolling, current_bytes):
      leaf = MAKE_LEAF(current)
      child_ref = STORE(leaf)
      output.push(child_ref)
      current = []
      rolling.reset()
      current_bytes = 0
  if current not empty:
    leaf = MAKE_LEAF(current)
    output.push(STORE(leaf))
  return output

Rust-like:

pub async fn build_leaf_level<I>(
    &self,
    items: I,
) -> Result<Vec<RopeChild>>
where
    I: IntoIterator<Item = T>,
{
    let mut children = Vec::new();
    let mut buf = Vec::new();
    let mut rolling = RollingHash::new();
    let mut logical_bytes = 0u64;
    let mut encoded_bytes = 0usize;
    for item in items {
        let mut boundary = Vec::new();
        item.boundary_bytes(&mut boundary);
        rolling.push(&boundary);
        logical_bytes += item.logical_len_bytes();
        encoded_bytes += estimated_encoded_len(&item);
        buf.push(item);
        if self.should_split_leaf(
            rolling.value(),
            buf.len(),
            encoded_bytes,
            logical_bytes,
        ) {
            let leaf = make_leaf(std::mem::take(&mut buf))?;
            let child = self.store_leaf(leaf).await?;
            children.push(child);
            rolling.reset();
            logical_bytes = 0;
            encoded_bytes = 0;
        }
    }
    if !buf.is_empty() {
        let leaf = make_leaf(buf)?;
        let child = self.store_leaf(leaf).await?;
        children.push(child);
    }
    Ok(children)
}

11.2 Build parent levels

Formal algorithm:

BUILD_PARENT_LEVEL(children, level):
  current = []
  rolling = new rolling hash
  for child in children:
    append child to current
    rolling.push(child.digest + child.total_bytes + child.total_items)
    if SHOULD_SPLIT_INTERNAL(current, rolling):
      node = MAKE_INTERNAL(level, current)
      output.push(STORE(node))
      current = []
      rolling.reset()
  if current not empty:
    node = MAKE_INTERNAL(level, current)
    output.push(STORE(node))
  return output

Rust-like:

pub async fn build_parent_level(
    &self,
    input: Vec<RopeChild>,
    level: u8,
) -> Result<Vec<RopeChild>> {
    let mut output = Vec::new();
    let mut buf = Vec::new();
    let mut rolling = RollingHash::new();
    for child in input {
        rolling.push(&boundary_bytes_for_child(&child));
        buf.push(child);
        if self.should_split_internal(rolling.value(), buf.len()) {
            let node = make_internal(level, std::mem::take(&mut buf))?;
            let parent_ref = self.store_internal(node).await?;
            output.push(parent_ref);
            rolling.reset();
        }
    }
    if !buf.is_empty() {
        let node = make_internal(level, buf)?;
        let parent_ref = self.store_internal(node).await?;
        output.push(parent_ref);
    }
    Ok(output)
}

11.3 Build root

pub async fn build_from_items<I>(&self, items: I) -> Result<BlockId>
where
    I: IntoIterator<Item = T>,
{
    let mut level = 0u8;
    let mut children = self.build_leaf_level(items).await?;
    if children.is_empty() {
        let leaf = make_leaf(Vec::<T>::new())?;
        let child = self.store_leaf(leaf).await?;
        return Ok(child.block_id);
    }
    while children.len() > 1 {
        level += 1;
        children = self.build_parent_level(children, level).await?;
    }
    Ok(children[0].block_id)
}

Complexity:

Time: O(n)
Space during build: O(width of current level)
Stored metadata: O(number_of_items / leaf_items)

⸻

12. Algorithm 2: lookup by byte offset

Input:

root, byte_offset

Output:

chunk containing that byte

Formal algorithm:

GET_AT_BYTE(node_id, offset, global_start, item_index_base):
  node = LOAD(node_id)
  if node is leaf:
    for each entry:
      if offset < entry.len:
        return cursor(entry, item_index, global_start, offset)
      offset -= entry.len
      global_start += entry.len
      item_index += 1
  if node is internal:
    for each child:
      if offset < child.total_bytes:
        return GET_AT_BYTE(child.block_id, offset, global_start, item_index_base)
      offset -= child.total_bytes
      global_start += child.total_bytes
      item_index_base += child.total_items

Rust-like:

pub async fn get_at_byte(
    &self,
    root: BlockId,
    mut offset: u64,
) -> Result<Option<RopeCursor<T>>> {
    let mut node_id = root;
    let mut global_start = 0u64;
    let mut item_index_base = 0u64;
    loop {
        let node = self.store.get_node::<T>(&node_id).await?;
        match node {
            RopeNode::Leaf(leaf) => {
                if offset >= leaf.total_bytes {
                    return Ok(None);
                }
                let mut local_offset = offset;
                let mut item_start = global_start;
                for (i, item) in leaf.entries.into_iter().enumerate() {
                    let len = item.logical_len_bytes();
                    if local_offset < len {
                        return Ok(Some(RopeCursor {
                            item,
                            item_index: item_index_base + i as u64,
                            item_start_byte: item_start,
                            offset_inside_item: local_offset,
                        }));
                    }
                    local_offset -= len;
                    item_start += len;
                }
                return Ok(None);
            }
            RopeNode::Internal(internal) => {
                if offset >= internal.total_bytes {
                    return Ok(None);
                }
                let mut found = false;
                for child in internal.children {
                    if offset < child.total_bytes {
                        node_id = child.block_id;
                        found = true;
                        break;
                    }
                    offset -= child.total_bytes;
                    global_start += child.total_bytes;
                    item_index_base += child.total_items;
                }
                if !found {
                    return Ok(None);
                }
            }
        }
    }
}

Complexity:

O(tree_height + items_inside_leaf)

With sane leaf sizes:

O(log n)

⸻

13. Algorithm 3: slice by byte range

Input:

root, byte range [start, end)

Output:

ordered LogicalChunkRefs overlapping the range

Formal algorithm:

SLICE(node, query_start, query_end, node_start):
  node_end = node_start + node.total_bytes
  if query_end <= node_start or query_start >= node_end:
    return []
  if query_start <= node_start and node_end <= query_end:
    return all items under node
  if leaf:
    scan entries and return overlapping entries
  if internal:
    recurse into children with accumulated offsets

Rust-style shape:

pub async fn slice_by_bytes(
    &self,
    root: BlockId,
    range: std::ops::Range<u64>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    self.collect_range(root, 0, range, &mut out).await?;
    Ok(out)
}

The optimization is important:

If entire subtree is inside range:
  append all leaf entries under subtree

For restore planning, you often want a stream, not Vec<T>, to avoid large memory allocations.

Long-term API:

pub async fn iter_range(
    &self,
    root: BlockId,
    range: Range<u64>,
) -> Result<RopeRangeIterator<T>>;

⸻

14. Algorithm 4: split at byte offset

To support replace/insert/delete, define:

split_at(root, offset) -> (left_root, right_root)

Meaning:

sequence(root) = sequence(left_root) || sequence(right_root)
left_root contains bytes [0, offset)
right_root contains bytes [offset, end)

There is one subtle issue: offset may fall inside a chunk.

For large-file recipes, you usually want operations aligned to chunk boundaries. In many ingestion flows, replacement ranges are chunk-aligned because they come from CDC output.

So define two variants:

split_at_item_boundary(root, item_index)
split_at_byte_boundary(root, offset, mode)

Where mode is:

pub enum SplitMode {
    RequireBoundary,
    IncludeContainingChunkLeft,
    IncludeContainingChunkRight,
}

For restore recipes, prefer chunk-aligned operations.

Formal split algorithm:

SPLIT(node, offset):
  if offset == 0:
    return (empty, node)
  if offset == node.total_bytes:
    return (node, empty)
  if leaf:
    split entries at item boundary
    build left leaf
    build right leaf
    return roots
  if internal:
    find child containing offset
    left_children = children before child
    right_children = children after child
    (child_left, child_right) = SPLIT(child, local_offset)
    return (
      CONCAT(left_children + child_left),
      CONCAT(child_right + right_children)
    )

In production, CONCAT must normalize boundaries near the join.

⸻

15. Algorithm 5: concat two ropes

Define:

concat(left_root, right_root) -> new_root

Naive concat can create bad trees.

Production concat should:

1. Preserve large unchanged subtrees.
2. Rebuild only a small boundary window.
3. Maintain node size limits.
4. Maintain content-derived boundaries.

High-level algorithm:

CONCAT(left, right):
  if left empty: return right
  if right empty: return left
  extract rightmost boundary spine from left
  extract leftmost boundary spine from right
  combine the small edge window
  rebuild that window using normal boundary rules
  reuse untouched interior nodes
  rebuild ancestors

A simpler v1 implementation:

1. Flatten only the edge leaves around the boundary.
2. Rebuild those leaves.
3. Reconstruct parent path.

This is enough to start.

For correctness-first implementation:

CONCAT_SIMPLE(left, right):
  stream all items from left and right
  build_from_items(stream)

This is O(n), but very simple and useful for tests.

For production:

CONCAT_LOCAL(left, right):
  reuse interiors
  rebuild local boundary window

Recommended implementation ladder:

Phase 1:
  concat_simple for correctness
Phase 2:
  concat_local for performance

⸻

16. Algorithm 6: replace range

This is the core mutation primitive.

replace_range(root, range, replacement_items) -> new_root

Formal:

REPLACE_RANGE(root, [start, end), replacement):
  (prefix, suffix0) = SPLIT(root, start)
  (_, suffix)       = SPLIT(suffix0, end - start)
  replacement_root = BUILD_FROM_ITEMS(replacement)
  return CONCAT(CONCAT(prefix, replacement_root), suffix)

This supports:

insert:
  replace_range(offset..offset, items)
delete:
  replace_range(start..end, [])
replace:
  replace_range(start..end, new_items)

Rust-like:

pub async fn replace_range(
    &self,
    root: BlockId,
    range: Range<u64>,
    replacement: Vec<T>,
) -> Result<BlockId> {
    let (prefix, suffix0) = self.split_at_byte(root, range.start).await?;
    let (_, suffix) = self.split_at_byte(suffix0, range.end - range.start).await?;
    let replacement_root = self.build_from_items(replacement).await?;
    let tmp = self.concat(prefix, replacement_root).await?;
    self.concat(tmp, suffix).await
}

For large-file version ingestion, you may not call this directly if the client only gives you a full new file. But it is very useful for VFS writes, patch checkout, and file-type-aware updates.

⸻

17. Algorithm 7: building a new version from a full chunk sequence

This is the common path:

new 10GB file
  -> CDC chunks
  -> LogicalChunkRef sequence
  -> build rope root

Naively:

let new_root = rope.build_from_items(new_chunks).await?;

This is O(n), but acceptable because CDC already scanned the 10GB file.

Because nodes are content-addressed, unchanged leaves can reuse existing block IDs automatically if boundaries match.

For better reuse:

1. Use stable boundary rules based on chunk_hash + raw_len.
2. Use canonical encoding.
3. Do not include version-specific data in nodes.
4. Do not include physical xorb locations.

Then unchanged spans produce identical leaf blocks and internal blocks.

⸻

18. Algorithm 8: diff two ropes

Diff is one of the biggest wins.

Input:

base_root, target_root

Output:

spans:
  Equal
  Insert
  Delete
  Replace

18.1 Span model

#[derive(Clone, Debug)]
pub enum RopeDiffSpan {
    Equal {
        base_offset: u64,
        target_offset: u64,
        len: u64,
    },
    Insert {
        target_offset: u64,
        target_len: u64,
    },
    Delete {
        base_offset: u64,
        base_len: u64,
    },
    Replace {
        base_offset: u64,
        base_len: u64,
        target_offset: u64,
        target_len: u64,
    },
}

18.2 Recursive diff

Formal:

DIFF(a, b, a_start, b_start):
  if a.block_id == b.block_id:
    emit Equal(a_start, b_start, a.total_bytes)
    return
  if both leaves:
    diff leaf entries by chunk_hash/raw_len
    emit spans
    return
  if levels differ:
    descend taller side
    return
  if both internal:
    align children by digest using ordered sequence diff
    recurse into changed child pairs

The hard part is internal child alignment.

Because a rope has no explicit keys, child lists can shift after insertions. So at each internal level you should run a small sequence diff over child digests.

Example:

base children:
A B C D E
target children:
A B X C D E

Child digest diff:

Equal: A B
Insert: X
Equal: C D E

Then you only descend into changed areas.

18.3 Leaf diff

For leaves, diff by item identity:

item identity = chunk_hash + raw_len

Example:

base leaf:
A B C D
target leaf:
A B X D

Leaf diff:

Equal A B
Replace C -> X
Equal D

Use a standard sequence diff algorithm:

Myers diff
Patience diff
Histogram diff

For large chunk recipes, leaf sizes are small enough that Myers on leaves is fine.

18.4 Internal child diff

At internal levels, use child digest as item identity:

child identity = child.digest + child.total_bytes + child.total_items

Then run sequence diff over child arrays.

Pseudo-code:

async fn diff_nodes(
    &self,
    a_id: BlockId,
    b_id: BlockId,
    a_start: u64,
    b_start: u64,
    out: &mut Vec<RopeDiffSpan>,
) -> Result<()> {
    if a_id == b_id {
        let a = self.store.get_node::<T>(&a_id).await?;
        out.push(RopeDiffSpan::Equal {
            base_offset: a_start,
            target_offset: b_start,
            len: a.total_bytes(),
        });
        return Ok(());
    }
    let a = self.store.get_node::<T>(&a_id).await?;
    let b = self.store.get_node::<T>(&b_id).await?;
    match (a, b) {
        (RopeNode::Leaf(a_leaf), RopeNode::Leaf(b_leaf)) => {
            diff_leaf_entries(a_leaf, b_leaf, a_start, b_start, out);
        }
        (RopeNode::Internal(a_int), RopeNode::Internal(b_int))
            if a_int.level == b_int.level =>
        {
            self.diff_internal_children(a_int, b_int, a_start, b_start, out).await?;
        }
        (a_node, b_node) => {
            self.diff_mismatched_levels(a_node, b_node, a_start, b_start, out).await?;
        }
    }
    Ok(())
}

18.5 Complexity

Best case:

roots equal:
  O(1)

Small change:

O(changed_path_count * height + local diff windows)

Worst case:

O(number_of_items)

But for versioned large files with stable chunking, you expect many equal subtrees.

⸻

19. Algorithm 9: restore planning from one version to another

This is the money path for Crab.

Input:

current_root: recipe for local materialized file
target_root: recipe user wants
current_file_path

Output:

RestorePlan:
  copy local ranges
  fetch missing chunks

Diff:

current: A B C D E F G H
target:  A B C D X F G H

Plan:

CopyLocal A B C D
Fetch X
CopyLocal F G H

Types:

pub enum RestoreOp {
    CopyLocal {
        src_offset: u64,
        dst_offset: u64,
        len: u64,
    },
    FetchChunks {
        dst_offset: u64,
        chunks: Vec<LogicalChunkRef>,
    },
    FetchXorbRange {
        xorb_id: XorbId,
        chunk_start: u32,
        chunk_end: u32,
        dst_offset: u64,
        raw_len: u64,
    },
}
pub struct RestorePlan {
    pub target_size: u64,
    pub ops: Vec<RestoreOp>,
    pub expected_file_hash: ContentHash,
}

Planning algorithm:

PLAN_RESTORE(current, target):
  diff = DIFF(current, target)
  for span in diff:
    if Equal:
      add CopyLocal
    if Insert or Replace:
      target_chunks = SLICE(target, target_span)
      add FetchChunks(target_chunks)
    if Delete:
      add nothing

Then optimize:

FetchChunks -> resolve chunk locations -> coalesce into xorb ranges

Pseudo-code:

pub async fn plan_restore_from_existing(
    &self,
    current_root: BlockId,
    target_root: BlockId,
    current_file_path: PathBuf,
    target_file_hash: ContentHash,
    target_size: u64,
    locator: &dyn ChunkLocator,
) -> Result<RestorePlan> {
    let diff = self.diff(current_root, target_root).await?;
    let mut ops = Vec::new();
    for span in diff {
        match span {
            RopeDiffSpan::Equal {
                base_offset,
                target_offset,
                len,
            } => {
                ops.push(RestoreOp::CopyLocal {
                    src_offset: base_offset,
                    dst_offset: target_offset,
                    len,
                });
            }
            RopeDiffSpan::Insert {
                target_offset,
                target_len,
            } => {
                let chunks = self
                    .slice_by_bytes(target_root, target_offset..target_offset + target_len)
                    .await?;
                ops.push(RestoreOp::FetchChunks {
                    dst_offset: target_offset,
                    chunks,
                });
            }
            RopeDiffSpan::Replace {
                target_offset,
                target_len,
                ..
            } => {
                let chunks = self
                    .slice_by_bytes(target_root, target_offset..target_offset + target_len)
                    .await?;
                ops.push(RestoreOp::FetchChunks {
                    dst_offset: target_offset,
                    chunks,
                });
            }
            RopeDiffSpan::Delete { .. } => {
                // Target output does not include deleted bytes.
            }
        }
    }
    let ops = optimize_fetch_ops(ops, locator).await?;
    Ok(RestorePlan {
        target_size,
        ops,
        expected_file_hash: target_file_hash,
    })
}

⸻

20. Algorithm 10: coalescing chunk fetches into xorb reads

Given chunks:

A B C X F G H

Resolve locations:

A -> xorb_1 index 0
B -> xorb_1 index 1
C -> xorb_1 index 2
X -> xorb_9 index 0
F -> xorb_2 index 1
G -> xorb_2 index 2
H -> xorb_2 index 3

Coalesce adjacent chunks in the same xorb:

xorb_1 chunks 0..3
xorb_9 chunks 0..1
xorb_2 chunks 1..4

Formal:

COALESCE(chunks):
  current = None
  for chunk in logical order:
    loc = lookup_best_location(chunk.hash)
    if current can extend with loc:
      extend current
    else:
      flush current
      current = new fetch range
  flush current

This produces efficient object-store reads.

⸻

21. Algorithm 11: merge three ropes

Merge input:

base_root
left_root
right_root

Output:

merged_root or conflict

Use span diffs:

left_diff  = DIFF(base, left)
right_diff = DIFF(base, right)

Then merge edits over base coordinate space.

Rules:

If left and right edit disjoint base ranges:
  apply both
If only left edits:
  use left
If only right edits:
  use right
If both edit same base range and produce identical target content:
  use either
If both edit same base range differently:
  conflict

Types:

pub enum RopeMergeResult {
    Clean { root: BlockId },
    Conflicted { conflicts: Vec<RopeConflict> },
}
pub struct RopeConflict {
    pub base_range: Range<u64>,
    pub left_range: Range<u64>,
    pub right_range: Range<u64>,
}

For large binary files, overlapping edits should usually conflict unless a file-type-specific plugin can merge them.

Example clean merge:

base:  A B C D E F
left:  A B X D E F     changed C
right: A B C D Y F     changed E
merged:
       A B X D Y F

Example conflict:

base:  A B C D
left:  A B X D
right: A B Y D

Both changed C.

Conflict.

⸻

22. Rust implementation plan

Now let’s turn this into a real implementation roadmap.

Phase 0: crate layout

Suggested crate:

fern-prolly/
  src/
    lib.rs
    hash.rs
    block_store.rs
    encoding.rs
    rope/
      mod.rs
      item.rs
      node.rs
      policy.rs
      builder.rs
      lookup.rs
      slice.rs
      split.rs
      concat.rs
      replace.rs
      diff.rs
      merge.rs
      restore_plan.rs
      validate.rs
      tests.rs

Or separate crates:

fern-block-store
fern-prolly-map
fern-prolly-rope
fern-large-file

For your platform, I would eventually split:

fern-prolly-core
fern-prolly-map
fern-prolly-rope
fern-xorb-store
fern-large-file

⸻

Phase 1: core block store and encoding

1.1 BlockStore

#[async_trait::async_trait]
pub trait BlockStore: Clone + Send + Sync + 'static {
    async fn get(&self, id: &BlockId) -> Result<Option<Bytes>>;
    async fn put(&self, bytes: Bytes) -> Result<BlockId>;
    async fn contains(&self, id: &BlockId) -> Result<bool>;
}

Provide implementations:

MemoryBlockStore
RocksDbBlockStore
S3BlockStore
DynamoDbMetadataStore if needed

1.2 Canonical encoding

Use a deterministic encoding.

Good options:

custom binary encoding
postcard
bincode with fixed options
protobuf with strict canonicalization

For content-addressed data, I prefer a custom binary format.

Encoding rules:

magic: "FROPE1"
node type: leaf/internal
level
total_bytes
total_items
entry count / child count
entries/children in order

Do not allow map/dictionary fields with unstable ordering.

1.3 Hash

Use BLAKE3 for speed unless you need SHA-256 compatibility.

pub fn hash_bytes(domain: &[u8], bytes: &[u8]) -> Hash256;

Use domain separation:

rope-leaf
rope-internal
rope-block
chunk-ref

⸻

Phase 2: node implementation

Implement:

impl<T: RopeItem> RopeLeaf<T> {
    pub fn new(entries: Vec<T>) -> Self;
    pub fn total_bytes(entries: &[T]) -> u64;
    pub fn digest(entries: &[T]) -> Hash256;
}
impl RopeInternal {
    pub fn new(level: u8, children: Vec<RopeChild>) -> Self;
}

Implement helper trait:

pub trait RopeNodeExt {
    fn level(&self) -> u8;
    fn total_bytes(&self) -> u64;
    fn total_items(&self) -> u64;
    fn digest(&self) -> Hash256;
}

Validation:

pub fn validate_node<T: RopeItem>(node: &RopeNode<T>) -> Result<()>;

Check:

totals match
digest matches
child count policy
level correctness

⸻

Phase 3: bulk builder

Implement:

pub async fn build_from_items<I>(&self, items: I) -> Result<BlockId>
where
    I: IntoIterator<Item = T>;

Also implement streaming builder for large files:

pub async fn build_from_stream<St>(&self, stream: St) -> Result<BlockId>
where
    St: futures::Stream<Item = Result<T>> + Unpin;

This matters because a 10GB file may have ~164k chunks; that is not huge, but streaming is cleaner.

Builder modules:

build_leaf_level
build_parent_level
store_leaf
store_internal

Test cases:

empty sequence
one item
exactly one leaf
many leaves
multi-level root
deterministic build
same input gives same root

⸻

Phase 4: lookup and slicing

Implement:

get_at_byte(root, offset)
get_at_index(root, index)
slice_by_bytes(root, range)
slice_by_items(root, range)
iter_items(root)
iter_range(root, range)

For first implementation, slice_by_bytes returning Vec<T> is fine.

For production, add iterators/streams:

pub struct RopeItemStream<T> {
    stack: Vec<TraversalFrame>,
}

Tests:

lookup first byte
lookup last byte
lookup across leaf boundary
lookup offset == total_bytes returns None
slice inside one leaf
slice across many leaves
slice full file
slice empty range

⸻

Phase 5: simple split/concat/replace

Start correctness-first.

5.1 Flatten-based implementation

For v1:

split:
  flatten all items
  split Vec
  build both roots
concat:
  flatten both
  build combined root
replace:
  flatten root
  replace range
  build new root

This is not production-performance, but it gives correct semantics and powerful tests.

It lets you stabilize APIs.

5.2 Localized implementation

Then replace internals with efficient versions.

Implement:

split_at_byte_local
concat_local
replace_range_local

Key ideas:

reuse unaffected subtrees
only flatten boundary windows
rebuild edge windows
preserve content-addressed blocks

The public API remains unchanged.

⸻

Phase 6: diff

Implement diff in layers.

6.1 Leaf diff

Use item identity:

fn item_identity<T: RopeItem>(item: &T) -> Hash256;

Run Myers diff for leaf entries.

For large leaves of 512 items, simple Myers is fine.

6.2 Child-list diff

For internal nodes, diff child arrays by:

child.digest + total_bytes + total_items

Again, child arrays are small.

6.3 Recursive diff

Implement:

pub async fn diff(
    &self,
    base: BlockId,
    target: BlockId,
) -> Result<Vec<RopeDiffSpan>>;

Tests:

identical roots -> one Equal
single replacement
single insertion
single deletion
multiple disjoint edits
large unchanged suffix
changed leaf only
changed internal subtree

⸻

Phase 7: restore planner

Build this in the large-file layer, not the generic rope layer.

Crate:

fern-large-file

Types:

pub struct LargeFileVersion {
    pub file_id: FileId,
    pub version_id: VersionId,
    pub file_size: u64,
    pub file_hash: ContentHash,
    pub recipe_root: BlockId,
    pub chunker_policy_id: ChunkerPolicyId,
}
#[async_trait::async_trait]
pub trait ChunkLocator {
    async fn lookup_best(&self, chunk_hash: ContentHash) -> Result<ChunkLocation>;
}

Planner:

pub struct RestorePlanner<R, L> {
    pub rope: R,
    pub locator: L,
}

Implement:

plan_cold_restore(target_root)
plan_warm_restore(current_root, target_root, current_file_path)

Cold restore:

slice whole target
resolve chunks
coalesce xorb ranges

Warm restore:

diff current vs target
Equal -> CopyLocal
Insert/Replace -> Fetch
Delete -> skip

⸻

Phase 8: merge

Implement three-way merge after diff is reliable.

First version:

Only auto-merge disjoint edits.
Overlapping edits conflict.

Later:

Allow file-type-specific merge plugins.

API:

pub async fn merge(
    &self,
    base: BlockId,
    left: BlockId,
    right: BlockId,
) -> Result<RopeMergeResult>;

⸻

Phase 9: performance work

Add:

node cache
block prefetch
parallel subtree loading
batch BlockStore get
streaming range iterator
parallel xorb fetch
copy_file_range / reflink restore ops

Important metrics:

rope.build.items_per_sec
rope.lookup.latency
rope.diff.nodes_visited
rope.diff.equal_subtrees_skipped
rope.restore.local_bytes_copied
rope.restore.remote_bytes_fetched
rope.restore.xorbs_touched
rope.fragmentation_score

⸻

23. Production implementation details

23.1 Caching

Use a node cache:

pub trait NodeCache {
    fn get(&self, id: &BlockId) -> Option<Arc<DecodedNode>>;
    fn insert(&self, id: BlockId, node: Arc<DecodedNode>);
}

Recommended:

moka cache
lru cache
custom shard LRU

Cache by BlockId.

23.2 Batch get

For diff, you often load sibling nodes. Add:

async fn get_many(&self, ids: &[BlockId]) -> Result<Vec<Option<Bytes>>>;

This helps for object stores and DynamoDB-style metadata stores.

23.3 Validation mode

During development:

validate every node before store
validate every node after load

In production:

validate sampled or debug mode

23.4 Corruption handling

Every block is content-addressed.

On load:

hash(bytes) must equal block_id

If not:

return corruption error

Every restore should verify:

chunk hash
final file hash

23.5 Root publication

Publication order:

1. Upload missing chunks/xorbs.
2. Publish chunk locations.
3. Store rope nodes.
4. Store LargeFileVersion.
5. Move branch/ref pointer.

The version ref must be last.

⸻

24. Complete example: from file to rope to restore

Suppose a file chunks into:

A B C D E F G H

Each item:

A = { chunk_hash: hash_A, raw_len: 64KiB }
...

Build rope:

root_v1
 ├── leaf_1: A B C
 ├── leaf_2: D E
 └── leaf_3: F G H

Version 2 changes E to X:

A B C D X F G H

New rope:

root_v2
 ├── leaf_1: A B C      reused
 ├── leaf_4: D X        new
 └── leaf_3: F G H      reused

Diff:

Equal   A B C D
Replace E -> X
Equal   F G H

Warm restore from v1 to v2:

CopyLocal: A B C D
Fetch:     X
CopyLocal: F G H

Cold restore v2:

Traverse target rope:
A B C D X F G H
Resolve:
A/B/C/D/F/G/H -> old xorbs
X             -> new xorb
Fetch and write in order.

⸻

25. What this gives you

A native Prolly Rope gives you:

1. Stable sequence representation
2. No offset-key churn
3. Fast byte-range lookup
4. Fast chunk-range slicing
5. Cheap metadata sharing across versions
6. Efficient version-to-version diff
7. Restore plans that copy local unchanged spans
8. Clean separation between logical recipe and physical xorb layout
9. Repacking without changing file version roots
10. A natural foundation for VFS/lazy hydrate

The most important long-term design decision is this:

Do not make chunk_hash, byte_offset, chunk_index, or xorb location the key.

Instead:

Use a native sequence tree.

The rope’s “key” is implicit logical position, resolved through:

child order
leaf entry order
subtree total_bytes
subtree total_items

That is the correct long-term abstraction for large-file version recipes.