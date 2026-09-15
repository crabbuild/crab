# Storage identities, records and LTX integration

[Index](README.md). All multi-byte binary fields below use big-endian encoding.
All JSON objects reject unknown fields, duplicate keys and oversized bodies.

## Identity and path codec

IDs are byte arrays: tenant/application/namespace/session/incarnation/request
IDs are 16 bytes; Cell IDs and BLAKE3 digests are 32 bytes. Crab resolves
repository names through its catalog to stable IDs and compiled namespace
capabilities. Partition bytes are at most 1,024 bytes. Define `LP(x) = u32(length(x)) || x`.

```text
cell_id = BLAKE3("crab.cell.v1\0" || tenant_id || app_id || namespace_id
                 || LP(partition))
shard = first_u64(BLAKE3("crab.shard.v1\0" || namespace_id || LP(scope))) % N
partition_for_shard = u32(shard)
```

KV routes by scope only; queue/workflow namespace shard counts are powers of
two in 1..4096 and cannot change after provisioning. Workflow routing hashes
workflow_id; queue send hashes producer_id. Claims explicitly address one shard;
the native supervisor cycles shards rather than making one cross-shard transaction.

Paths relative to a configured authoritative storage root:

```text
cells/v1/identity.json
cells/v1/apps/<app>/release.json
cells/v1/apps/<app>/releases/<digest>.json
cells/v1/apps/<app>/catalog/<00..ff>/head.json
cells/v1/apps/<app>/catalog/objects/<digest>.json
cells/v1/apps/<app>/cells/<cell>/control.json
cells/v1/apps/<app>/cells/<cell>/inc/<inc>/objects/<digest>.<kind>
cells/v1/apps/<app>/pins/<pin-id>.json
cells/v1/nodes/<session>.json
```

identity.json is immutable strict-created JSON with version=1, tenant and
application as hex32; it selects the one authorized tenant/application pair for
this configured Crab root. Reject other pairs before routing. Administrative
initialization retries read and adopt the winning IDs; they never overwrite it.

This identity owner is now implemented by `ApplicationIdentityStore`. It rejects
noncanonical JSON and another tenant/application winner, and constructs an
application-scoped `CellStorageLayout` only from the verified persisted value.

IDs in paths are lowercase fixed-width hex. `kind` is one of ltx, index, dir,
root or bundle. Encoders accept typed IDs, never concatenate caller path text.
`crab-storage` owns physical path construction; LTX receives a validated scoped
layout. ETags are mutation tokens, never content hashes. Origin control/catalog
reads bypass caches and staging; immutable reads may use verified caches.

## Control JSON v1

Required fields and exact representations:

| Field | JSON type and validation |
| --- | --- |
| version | integer 1 |
| cell | 64 lowercase hex characters; matches path |
| incarnation | 32 lowercase hex; immutable except explicit restore |
| epoch, revision, progress | Decimal strings, u64; revision/epoch >= 1 |
| state | recovering, serving, idle, tombstoned |
| owner | null or `{session: hex32, endpoint: string}`; endpoint <= 512 bytes |
| root | null or RootRef below |
| code | compiled module descriptor digest hex64 |
| schema | integer u32 >= 1 |
| next_due_ms | null or decimal i64 string >= 0 |

RootRef has exactly `digest` (hex64), `txid` (u64 decimal string), `checksum`
(16 lowercase hex), and `commit_sequence` (u64 decimal <= i64::MAX). RootRef
inherits Cell/incarnation from control; a native/peer typed reference includes
those identities so it cannot be used against a different layout.

Control body limit is 8 KiB. Serving requires owner and root; idle/tombstoned
require owner null. Recovering can have root null only before first initialized
database publication. A takeover of such a record preserves its incarnation
and initializes from the catalog's declared schema; it does not invent a new
incarnation. Every replacement validates the transition table in runtime.md.

Root updates must match actual SQLite sequence/schema and LTX position. Clock
or scheduling summary changes never replace a root independently of its data.
Renewals preserve the published next_due_ms value.

## Root codec and dependency graph

Root JSON fields: `version=1`, `cell`, `incarnation`, `txid`, `checksum`,
`commit_sequence`, `page_size`, `database_pages`, `schema`, `directory_digest`,
`directory_height` and `segment_pages`. Integer representations
match control (u64 decimal strings; page size/count/height are JSON integers).
Serialize keys lexicographically with no whitespace; hash the resulting bytes.
Readers verify those bytes' digest before decoding, without reserialization.

Root <= 32 KiB; at most 64 segment-page digests. Each immutable segment-page JSON
array has at most 96 ordered descriptors and is <= 64 KiB. This covers the 4,096
descriptor limit even at maximum canonical integer widths. Descriptor fields
are the existing SegmentInfo fields plus physical `object_digest`, `offset`,
`length`, `index_digest` and `index_length`. Native bodies have offset zero;
bundle bodies identify the exact original LTX extent. No body-location fallback.
Every size/offset addition uses checked arithmetic before I/O.

The first segment is a full snapshot; subsequent ranges are contiguous through
the exact root TXID. Verify pre/post checksum continuity and every decoded
body's BLAKE3, including intermediate states on full restore. Maintain a maximum
4,096 segment descriptors; at 3,072 schedule compaction aggressively, and at
4,096 reject new writes until maintenance frees descriptor capacity. Never grow
an unbounded linked list of predecessor roots.

Old immutable roots are retained; the new root reuses descriptor pages and page
directory nodes by digest. Compaction replaces descriptors at identical logical
sequence/schema, validates equivalent page state, then proposes a normal control
CAS. The control record is the only mutable authority for Cell ownership and roots.

## Authenticated page directory

Executable code is retained in container images, not in the LTX graph. The
release descriptor identifies compiled code; workflow_runs.definition_digest
selects retained native definitions. Neither is a downloadable module. Backups
record the required release descriptor digests separately from SQLite roots.
There is no sys_blob_refs table for deployment artifacts in this design.
Existing Crab Git/LFS/release-asset reference and GC policy stays with its owner.

Replace the fully resident locator map with a persistent radix tree. Leaf index
is `(page_number - 1) / 256`; branch fanout is 256. Leaves list actual page
numbers in strictly increasing order. A database page must have one locator
except SQLite's reserved lock page, handled by the existing VFS rule. Missing
allocated pages fail verification; holes never become zero-filled user data.

Binary node header is 32 bytes: magic `CRBDIR01` (8), version u16=1, kind u8
(leaf=0/branch=1), reserved u8=0, entry_count u32, live_pages u64, XOR page
checksum u64. No trailing bytes are accepted.

Leaf record is 88 bytes: page_number u32, physical_object_digest[32], absolute
frame_offset u64, frame_length u32, frame_BLAKE3[32], decoded_page_crc u64.
Branch record is 56 bytes: first_page u32, last_page u32, child_digest[32],
live_pages u64, XOR checksum u64. At most 256 records/node; tree ranges must
match radix boundaries without overlap. Parent aggregates equal XOR/count of
children. Top-level count/checksum must match the existing verified page-state
algorithm, including the LTX checksum flag/reserved-page rules.

Updating a cut loads only affected leaves and ancestors, verifies their hashes,
applies verified new page locators, removes truncated suffixes and recomputes
aggregates. Verify growth-page coverage before accepting the new root. Changed
nodes are copy-on-write immutable objects; original nodes remain usable by
historical roots. Initial snapshot construction streams sorted verified pages
into leaves; it does not allocate one locator per page.

Incremental cut update is now implemented: native indexes are decoded from the
already verified local cuts, affected radix paths are authenticated and rewritten,
whole truncated subtrees are dropped from parent aggregates without reading their
leaves, and untouched child digests remain shared with historical roots. Coverage
and the root checksum must equal the new LTX endpoint before `PreparedRoot` is
created. `changed_cut_loads_only_touched_directory_nodes` and
`truncate_regrow_cannot_reuse_old_locator` cover origin-read bounds and the
truncate/regrow safety invariant. Initial tree construction now k-way merges
authenticated index streams and uploads leaves as they are completed; it retains
the source index bytes but no complete locator map or directory body set.

The local capture checksum tracker is now a disposable fixed-width file created
from the authenticated directory during writable preparation. Construction emits
big-endian eight-byte entries in 64 KiB chunks and never downloads LTX bodies.
Each capture candidate clones only its changed-page overlay, reads old values for
changed pages or a removed truncation suffix, and maintains the aggregate XOR in
constant time per touched checksum. After the matching LTX cut is synced and
renamed, positional writes update the file and `sync_all` completes before the
capture position advances. Any failure fences the session; the local file is
never publication authority and is rebuilt for a new exact-root activation.

The resident node cache now uses an 8 MiB process-wide byte ceiling, keyed by a
non-reusable Store instance identity, complete typed Cell/incarnation object path
and node digest. It inserts only after BLAKE3 verification; FIFO eviction loses
only verified cached bytes and a different backing Store cannot reuse an entry.
Disk cache uses the same key and verifies BLAKE3 after reopening. Fault reads
coalesce adjacent frames from one immutable object into at most 1 MiB per range,
then verify every frame hash, decoded page identity and CRC before installation.
Hydration and foreground writes retain the existing writable-VFS overwrite and
truncate ordering. Concurrent hydration cannot reinstall an obsolete page.

## LTX APIs to implement

```rust,ignore
pub struct RootRef {
    pub cell: [u8; 32],
    pub incarnation: [u8; 16],
    pub digest: [u8; 32],
    pub position: crab_ltx::Position,
    pub commit_sequence: u64,
}
pub struct PreparedRoot {
    predecessor: Option<RootRef>,
    root: RootRef,
}
impl CellReplica {
    pub async fn prepare(
        &self, base: Option<&RootRef>, cuts: &CaptureBatch,
        sequence: u64, schema: u32,
    ) -> Result<PreparedRoot>;
    pub async fn open_root(&self, root: &RootRef) -> Result<VerifiedRoot>;
    pub async fn prepare_bundle(
        &self, base: Option<&RootRef>, bundle: &crab_ltx::bundle::Bundle,
        sequence: u64, schema: u32,
    ) -> Result<PreparedRoot>;
    pub async fn prepare_compaction(
        &self, base: &RootRef, range: std::ops::Range<usize>, level: u8,
    ) -> Result<PreparedRoot>;
}
impl VerifiedRoot {
    pub async fn restore(&self, destination: &std::path::Path)
        -> Result<crab_ltx::Position>;
}
```

Native and bundled append preparation share the same segment/index verification
and immutable-root builder. `prepare` uploads immutable dependencies only: no
per-epoch head write. It
accepts a verified historical root without a mutation token; authority remains
the runtime's responsibility. New incarnation uses `base=None` and a complete
snapshot. Existing incarnation continues exact TXID/checksum without epoch
renumbering. Ownership epoch is not the physical LTX namespace in this format.

`BundleEntry::for_cell` writes the canonical Cell/incarnation routing identity;
`prepare_bundle` filters those rows from a shared multi-Cell envelope, verifies
their exact chain, stores the complete bundle by digest and records absolute
frame offsets with no native-object fallback. `prepare_compaction` verifies the
selected bodies and their pinned indexes, compacts the exact range, replays only
suffix indexes needed to replace final directory locators and preserves the
base TXID, checksum, commit sequence and schema. The current compactor retains
selected inputs in memory; the bounded external merge below remains required
for the 5,000 MB qualification gate.

`PreparedRoot` fields are private; constructors must verify scope, complete
dependency upload, cut continuity and root metadata. The runtime cannot build
an unchecked prepared value. Maintenance and bundled appends use the same path.
Keep checksums mandatory and existing LTX sized-block encoding. This new root/
directory format uses a separate versioned namespace; hard cutover is explicit.

Extend ManagedDb transaction handling to preserve service errors instead of
coercing them into rusqlite errors: `TransactionError<E> = Operation(E) | Sqlite
| Capture`, preserving sources. Capture after local commit stays separately
owned by the actor. The runtime now layers bounded authorizer-protected helpers
over the worker callback; the remaining typed contexts must expose only those
helpers, never a raw connection that can alter pager settings through application
SQL or private peer operations.

## Streaming and scratch

Use 8 MiB I/O buffers and a 64 MiB memory reservation per capture/recovery/
compaction job, excluding the shared caches. Oversized encoded/decode units fail
admission. Snapshot/restore write sequentially to exclusive scratch files and
sync file then parent before installation. At 5,000 MB, reserve two database
sizes plus 64 MiB scratch before full recovery/compaction; sparse activation
reserves only dirty-page/WAL budgets plus bounded metadata.

Exact Cell-root restore now holds the Host recovery permit, fetches authenticated
adjacent-frame runs capped at 1 MiB, writes them sequentially through the Host
executor to an exclusive private same-directory scratch file, and independently
reduces the page checksums to the root position. It verifies final length, syncs
the file, hard-links it into the absent destination, syncs the parent, and removes
the scratch name. Existing destinations and SQLite sidecars fail before remote
page reads; cancellation or verification failure removes the owned scratch best
effort and never replaces another file. The 5,000 MB resource/RSS gate remains
unqualified; this implementation removes the whole-database restore buffer but
does not claim that capacity result.

Compaction performs external merge by page number through bounded scratch runs,
choosing the last page in the selected TXID range. Verify output against an
independent reduction/directory aggregate before preparation. Never materialize
the full database or all input LTX bytes in Vec buffers. Only selected bodies
download; unselected authenticated descriptors remain referenced.

## Catalog and startup

Catalog shard is first byte of Cell ID. Its CAS head contains version, revision
and up to 256 immutable page digests; each sorted page holds up to 256 entries
`{cell, namespace, partition, role, initial_code, initial_schema}`. A shard
caps at 65,536 entries; return RESOURCE_EXHAUSTED at this v1 bound. Writers CAS
the head after uploading changed pages, with ID collision checks. Provision
catalog before control so crashes can leave only harmless empty entries.

This format is implemented by `CellCatalog`. Head bodies are canonical strict
JSON bounded to 32 KiB: `version` is integer 1, `revision` is a nonzero canonical
u64 decimal string, and `pages` contains 1–256 lowercase BLAKE3 hex digests.
Page bodies are canonical strict JSON bounded to 1 MiB with `version=1` and
1–256 entries. Entry IDs/digests and partition bytes are lowercase hex; roles
are `repository`, `sql`, `kv`, `queue`, or `workflow`. Readers verify the page
body against its head digest, recompute every Cell ID from the configured tenant/
application plus namespace/partition, and enforce global Cell ordering across
page boundaries before returning `CatalogProof`.

Provisioning reloads the whole bounded shard, rejects a different bootstrap
contract for an existing Cell ID, uploads every changed immutable page with
create-if-absent semantics, then strict-creates or ETag-updates the head. On a
failed head response it reloads and accepts only the exact requested entry;
state-dependent conflicts merge the winning entries and retry.
`CellAuthority::create_initial` requires this unforgeable proof and adopts a lost create response
only when the complete control bytes match. `CellRuntime` binds a local session
at construction and rejects activation unless the control owner names it.

Startup validates provider strict-create and failed-update behavior in a private
probe prefix, loads release/catalog roots, opens local capacity budgets, then
accepts traffic. Cell activation reserves capacity, acquires control, opens its
exact root with the managed sparse VFS and checks sys_meta identity/sequence/schema.
Local files are a cache; v1 never resumes arbitrary surviving WAL after restart.
