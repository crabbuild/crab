# crab-metadata

`crab-metadata` defines the schemas, codecs, indexes, and validation rules
that describe a Crab repository in object storage. It keeps the mutable
manifest small and makes large shard, pack, file, and chunk indexes immutable,
addressable, and independently verifiable.

## Why it exists

Git objects and Xet data are content-addressed, but a repository still needs a
consistent answer to questions such as:

- Which ref generation is current?
- Which shards and packs belong to that generation?
- Where can a file or chunk be reconstructed?
- Which receipts prove that an object was uploaded and committed?

Centralizing these payload contracts prevents readers and writers from
silently disagreeing about keys, generations, hashes, or serialization.

## Architecture

```text
{repo}/manifest                  mutable CAS root
        │ points to
        ├── segmented shard index immutable bulk object
        ├── segmented pack index  immutable bulk object
        ├── commit graph / refs   optional bulk summaries
        └── file_index_db         repo-scoped SlateDB index

.crab/chunk_index_db/             bucket-global chunk receipts and placements
```

`Manifest` is version 1 and contains the complete ref map, HEAD, generation,
and content hashes for larger metadata objects. `seal_git_validation` binds
the semantically validated Git state to a BLAKE3 digest; readers call
`validate_manifest_payload` before trusting refs or index pointers.

Capsule protocol v2 uses one compacted root plus independently mutable ref
heads and immutable capsule runs. Pointer catalogs keep shard/xorb payloads
external while authenticating their identities and reconstruction closure.
Readers that already loaded and verified a root use
`load_pointer_catalog_from_root`; this preserves the same catalog validation
without issuing a second mutable-root request.
Catalog readers resolve only ref heads in that root's authority epoch, before
loading multi-ref activation records. Restore can leave retired heads in storage;
their checkpoint positions and dependencies must not enter the restored catalog.
Current-epoch activation and checkpoint-chain errors still fail closed.
`v2/browse-indexes` is a bounded, mutable derived record, not ref authority.
It binds complete immutable commit-graph and path-state descriptors to the exact
capsule state, including visible per-ref transactions. Readers ignore stale
records; absent indexes remain a readiness state. Rebuilding an immutable index
verifies the generated content identity and repairs corrupt stored bytes only
with a conditional update against the observed version, followed by readback.
This applies to graph/path-state descriptors and layers, not Git/Xet data.
Graph/path-state loaders admit descriptor bodies against the caller's byte
budget before buffering, then admit each layer against its authenticated size
and the remaining aggregate budget. All bodies still require hash and exact
length verification; malformed provider sizes cannot bypass the encoded-byte
ceiling. Decoded index structures require separate memory qualification.
Path-state construction and in-memory validation need no storage feature;
encoded-layer decoding is private to storage loading and codec tests.
Current file lookup selects the protocol from the root alone: only an absent
v2 root permits v1 lookup. A missing v2 dependency remains an error, and failed
shared-session initialization can retry after that dependency is repaired.

Layered checkpoint visibility decoding is shared by readers and recovery.
Checkpoint pointers require an explicit format 5 (`CRBCKP05`). Root, history
and ref-head decoding reject retired or missing formats; there is no embedded
checkpoint decoder. The separately released v1 manifest protocol is unchanged.
Ordinal proofs must match the ordered source-catalog digest before becoming a
Git visibility index; the existing full-object encoding preserves the same
caller-supplied Git identity. A missing proof remains distinct from an empty
ref set, so recovery can reject it explicitly.
Footer-only checkpoints have control offset zero when both optional catalog and
visibility sections are absent. The storage reader accepts that canonical range
while retaining footer-hash, pointer identity, source and size validation.
Full reads, footer-only reads and retained maintenance checkpoints share one
complete pointer-identity comparison, including counts and covered-root binding.

History segments retain a checkpoint and the transactions folded into it.
Metadata-only publication may fold zero new transactions: its checkpoint still
owns the complete pack-source closure. Zero-transaction segments retain exact
refs and compacted positions and participate in the same authenticated history
chain; GC must traverse their checkpoint sources even without capsule runs.
Root, history and per-ref head admission share the same pointer validators.
They check the stored checkpoint format and capsule count directly, including
prepared heads and before following history dependencies; rebuilding a pointer
would hide those malformed fields.
Run pointers require explicit control offsets, lengths and footer hashes; the
unshipped offset-discovery shape is rejected. Full and control-only storage
reads validate descriptors before I/O and bind the loaded run to the same
control boundary and footer hash. This does not change the v1 manifest reader.

Run compaction preserves exact Git object-to-member admission across ref-only
runs. Their authenticated empty pack directory proves an empty contribution;
a pack-bearing run without admission still prevents a complete merged proof.
`CapsuleRun::compact` consumes ordered runs with a bounded power-of-two total
capsule count and encodes/authenticates the final run once. Mixed-level carries
retain canonical capsule bytes and member ordinals without encoding discarded
intermediate runs; complete capsule verification remains mandatory.
Runs may contain byte-identical packs from different transactions. Their source
directories retain every physical member and ordinal; identical content is
deduplicated only by readers. Source validation and readers share the same
content comparison, rejecting conflicting range lengths/hashes, sidecars, Git
checksums, object counts or external delta bases without comparing offsets.

`CRBRUN06` compacted runs also concatenate copies of their Git indexes into an
authenticated lookup pool. Original capsules and canonical member ranges stay
unchanged for recovery, installation and repack. Control-only readers derive
pool ranges in member order with the original index hashes; full decoding checks
the pool and every copied index. Leaves do not duplicate indexes. The run's
authenticated control suffix covers its exact member-admission bytes together
with the footer, so control loading needs no second admission request. The
decoder verifies the entire admission hash and exact suffix boundary before
exposing placement hints. Payload bytes and the lookup pool remain outside the
suffix. Detached large visibility/catalog sections retain their separate bounded
reads. The unshipped `CRBRUN04`/`CRBRUN05` development formats are rejected, not
read through a compatibility path; readers and writers must cut over together.

Payload modules cover manifests, segmented lists, pack metadata, commit-graph
summaries, ref registries, chunk/file indexes, receipts, transactions, and
canonical key/value codecs. Storage-backed helpers are feature-gated:

| Feature | Adds |
| --- | --- |
| `storage` | Object-store manifest, segmented-index, and prefilter helpers |
| `file-index-reader` | Read-only file-index lookup sessions |
| `local-index` | SQLite-backed local chunk index |
| `remote-index` | SlateDB remote index readers and writers |

## Reader and writer ownership

Keep each SlateDB session's lifecycle explicit: every opened reader or writer
must be closed on success and error paths.

`RemoteIndexWriter` opens only the indexes selected by its caller. Nonempty
entries for an unopened index are rejected before either batch is written.

| Operation | Guarantee |
| --- | --- |
| `RemoteIndexWriter::write_entries` | Buffer entries in opened databases; no per-batch durability guarantee. |
| `RemoteIndexWriter::close` | Attempt to flush and close both databases; return the file-index error first if both fail. |
| `write_index_entries` | Open the needed databases, write one batch, and close; preserve a write error over a close error. |

Always await close after a write error too. These operations do not provide an
atomic transaction across the two databases, and dropping their futures does
not provide asynchronous cleanup.

### Shared lookup lifecycle

`SharedFileIndexLookup::new_for_storage` opens one lazy session for concurrent
lookups. Initialization is shared; a slow first canonical shard scan does not
hold an exclusive session lock and block unrelated acceleration-index hits.
Canonical scans still serialize through their session cache.

Await `close()` after the operation's readers finish. Close rejects new lookups
and waits for active lookups before closing SlateDB, even if handle clones
remain. Concurrent close calls also wait for reader cleanup. Await close to
completion; dropping the owner or close future cannot perform asynchronous
cleanup. Scoped stores retain write-free canonical reads.

### Snapshot-bound lookup

Integrity callers use `FileIndexLookupSession::from_snapshot` with an already
captured `RepositorySnapshot` and its scoped storage layout. This constructor
does not open SlateDB, refresh metadata, or write reader checkpoints. Lookup
uses canonical shards, including committed journal additions; duplicate recipes
select the smallest shard hash deterministically. The caller still owns
freshness revalidation and protection against concurrent GC. This path scans
the captured shard inventory, so it is not an acceleration-index performance
claim.

### Git object catalog checkpoints

Each published Git object catalog checkpoint has a small identity marker written
after SlateDB makes the checkpoint durable.
`GitObjectLocatorSession::latest_published_identity` reads that marker without
opening the large catalog.
Callers may use the identity only after proving its immutable pack inventory is
covered by their pinned repository snapshot, then open the named checkpoint for
the actual lookup. A missing marker means publication is incomplete; malformed
or name-mismatched marker content fails closed.

### Snapshot identity

`RepositorySnapshot` also captures the validated canonical layout descriptor.
The reader validates it around metadata materialization, and the snapshot
digest binds its semantic content along with the complete manifest, CAS token,
and journal frontier. Equivalent JSON formatting or a new layout ETag does not
change this identity; missing, malformed, unsupported or oversized descriptors
fail closed. A missing manifest still returns its ordinary not-found error;
neither snapshot reads nor raw manifest creation initialize a layout.

The snapshot digest does not replace the manifest's Git-only validation digest.
Callers bind the repository namespace separately and revalidate the snapshot;
neither digest reserves physical dependencies against GC.

Receive validation uses
`FileIndexLookupSession::for_snapshot(&layout, &snapshot, limits)` when the
composing operation must supply stricter aggregate bounds.

### Lookup resource limits

| Limit | Scope |
| --- | --- |
| `max_files` | Each batch and the session's cached distinct files. |
| `max_shard_visits` | Cumulative shard visits, reserved before a scan is dispatched. |
| `max_shard_bytes` | Each fetched shard body. |
| `max_recipe_entries` | Expanded recipe entries. |

The captured inventory must fit the visit budget before session creation.
Failed or cancelled scans consume their reservation and cannot cache absences.
Cached results need no further shard visits.

At most four shard scans overlap across all sessions in a process. A scan
acquires capacity before origin I/O, then moves its permit into the blocking
hash/recipe parser. Dropping or timing out the caller leaves that permit held
until the worker exits; hashing and parsing stay off async workers. Explicit
session close drains these parsers before closing its reader or returning.

Excluding transport retries, the read budget is:

```text
shard bodies ≤ max_shard_visits × max_shard_bytes
per visit:   ≤ one HEAD + 12-byte trailer + 4 KiB bloom prefilter
```

A selected shard is only a dependency candidate. Verify the file's content at
origin with `crab-read::pointer_proof`, and hold GC fences and recheck the exact
publication base before accepting a write. The composing request must still own
an overall deadline and admission for the other receive stages. A timed-out
lookup may finish its current bounded parser in the background, retaining its
permit until completion; the caller must await session close before releasing
its operation resources. Pointer content proofs have their own four-operation
process bound and include admission queue time in their deadline.

## Usage

Create and validate a manifest payload without enabling any storage runtime:

```rust
use crab_metadata::manifests::{Manifest, validate_manifest_payload};

fn example() -> Result<(), Box<dyn std::error::Error>> {
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.refs.insert(
        "refs/heads/main".into(),
        "0000000000000000000000000000000000000000".into(),
    );
    manifest.seal_git_validation();
    validate_manifest_payload(&manifest)?;
    Ok(())
}
```

For remote indexes, enable the feature in a consuming Crab workspace member
(this crate is not published to the registry), then construct a repo-aware
layout for the lookup or write helpers:

```toml
[dependencies]
crab-metadata = { workspace = true, features = ["remote-index"] }
```

```rust
use crab_metadata::remote_index::RemoteIndexConfig;

let indexes = RemoteIndexConfig::for_repo("repositories/team/project");
assert_eq!(indexes.chunk_index_path, ".crab/chunk_index_db/");
```

## Boundaries

- [`crab-types`](../crab-types/README.md) owns cross-crate hash, storage, and
  replication types.
- [`crab-storage`](../crab-storage/README.md) owns object reads, writes, and
  CAS; metadata defines the objects and keys stored through it.
- [`crab-read`](../crab-read/README.md) owns reconstruction orchestration;
  this crate owns the metadata it consumes.
- [`crab-coordination`](../crab-coordination/README.md) decides when a new
  manifest generation is authoritative.

## Unborn default branches

A manifest or replayed ref journal may retain a symbolic `refs/heads/...` HEAD
that has no commit while tags or other branches exist. The validation digest still
binds HEAD and every ref; an unresolved non-branch HEAD remains invalid for a
nonempty repository. Read-side name and object validation remains mandatory.

This extends the readable states of the existing serialized schema. Deploy the
updated readers and publication services together: v1.0.1 and v1.1.0 reject this
state. Existing manifests with resolved HEADs require no migration.

`SharedFileIndexLookup::for_shard_index` lazily opens the canonical shard lookup
from a caller-captured immutable root and generation. It never reads the latest
manifest or opens SlateDB. The root cardinality is checked before segment
fetches, and the normal verified segment reader is reused. Clones share lookup
state until explicit close; scope and content retention remain the caller's
responsibility.
