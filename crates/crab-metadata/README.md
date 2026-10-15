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
`capture_ref_heads` and `resolve_referenced_activations` own that stable capture
for metadata and read orchestration. Transactionless heads must match the
compacted root; selected-ref capture reuses the same activation validator while
keeping its targeted ETag probes. Current-state catalog lookups reuse that owner together with the shared
control-selection and visibility machinery; full maintenance materialization
remains explicit.
Run controls retain each capsule's own pack directory in capsule-relative
coordinates. The run directory keeps physical transport coordinates separately.
Logical capsule equality compares authenticated visibility section identities
and page roots rather than cache state or transport origin. Identical shared
capsules can therefore appear in differently compacted ref frontiers without
inflating pack counts or appearing to conflict; differing payload commitments
remain rejected.

Checkpoint format 7 (`CRBCKP07`) carries authenticated catalog page roots in
its footer. `LayeredCheckpoint::pointer_catalog_lookup` captures these roots;
`PointerCatalogLookup` batches file/chunk/xorb keys through one bounded page
cache and byte budget. Its `dependencies` query selects files plus explicit
shard/xorb identities in one aggregate budget, including complete dependency
closure. Missing candidate identities remain misses; missing referenced
artifacts and conflicting visible descriptors fail. Full inspection remains
an explicit maintenance operation. `SharedFileIndexLookup::for_pointer_catalog`
accepts captured lookup roots and resolves requested files with their authenticated
shard/xorb closure. Point and batch calls share the page cache and retain the
session read guard through I/O, so close waits for active reads. Missing catalog
pages remain errors rather than reopening the mutable file-index database.
A session caches at most 4,096 verified catalog bindings or authenticated misses,
using the same result-cache owner as canonical shard lookup. Cold queries still
validate the full selected dependency closure before publishing cache entries;
errors are never cached as absence. Cached inputs retain the original admission
charge. Catalog cache locks cover memory work only, so unrelated warm lookups can finish
while a cold origin read is paused. Retention counts distinct file identities;
duplicate occurrences do not consume additional entries or prevent caching.
The current-state opener also captures authenticated catalog roots and leaves
catalog bodies cold. Explicitly pinned callers pass their existing capture, while
current-state sessions bind once to the first root they open. Catalog and Git
views share stable ref capture, ref-frontier selection, visibility reconciliation
and transaction ordering; neither path invents a separate precedence policy. All 19 audited local release tags omit these
capsule checkpoint formats; unshipped format 6 is rejected without a fallback.
Checkpoint and selected run catalog roots share one lookup budget and page cache.
CLI and shared capsule-publication preparation use these captured queries.
Protected receive selects from separately authorized source/filtered captures.
Captured revision, remote-mount and current-state hydration use the shared paged
lookup. Control capture and selected closure queries are measured separately;
complete opening and history-amplification qualification remain required.

External pointer catalogs use `CRBPCP01` authenticated pages. A bounded descriptor
commits file, shard, xorb and reverse-chunk directories; nested shard closures and
ordered xorb chunk lists are paged too. Keys preserve canonical lowercase Xet hex
ordering, including the previous lowest-xorb/first-occurrence placement rule.
Complete inspection verifies the body hash, complete dependency closure and
canonical page layout. Delta inspection defers base-owned closure to `apply`,
which retains later file bindings at equal size and rejects immutable shard/xorb
conflicts.
Full inspection of a captured visible set uses `PointerCatalogAssembly`: binding
order need not be dependency order across independent refs. Rows and immutable
conflicts are checked while collecting deltas, then the complete closure is
checked before returning the catalog. Publication-time `PointerCatalog::apply`
still requires each delta's dependencies to exist in its base. Encoded catalogs are limited to 2 GiB and descriptors to 4 KiB.
The retired monolithic JSON encoding was absent from all 19 audited release tags
and is rejected. Shared file lookup uses paged capture for pinned and current-state sessions.
Complete catalog inspection remains explicit; consumer, historical amplification
and lifecycle qualification must not be inferred from the page representation.

`v2/browse-indexes` is a bounded, mutable derived record, not ref authority.
Its version 3 record binds a durable immutable commit graph, optional complete
physical object lookup and optional complete path attribution to the exact
capsule state, including visible per-ref transactions. Physical lookup readiness
requires complete verified pack coverage before publication; path completion
retains that binding. Graph publication does not wait for path construction. The prior
versions were never reachable from a release tag; maintenance rebuilds this derived
record in place, without a legacy reader. Readers ignore stale
records; absent indexes remain a readiness state. GC retains dependencies of the currently
published readiness record even when newer refs make it unusable for new reads.
Storage retention does not grant inventory membership or ref visibility.
Rebuilding an immutable index
verifies the generated content identity and repairs corrupt stored bytes only
with a conditional update against the observed version, followed by readback.
This applies to graph/path-state/physical-index descriptors and bodies, not Git/Xet data.
Graph/path-state loaders admit descriptor bodies against the caller's byte
budget before buffering, then admit each layer against its authenticated size
and the remaining aggregate budget. All bodies still require hash and exact
length verification; malformed provider sizes cannot bypass the encoded-byte
ceiling. Decoded index structures require separate memory qualification.
Capsule browse construction writes split-graph descriptor version 3, with
authenticated ordinal-record and OID-directory pages and a verified positional
digest. Per-commit ancestor roots and content-addressed interval nodes share
unchanged subtrees and collapse full ordinal ranges. Maintenance recomputes the
sets from the complete verified DAG and checks their canonical first-use layer;
local page hashes alone cannot certify ancestry. Point membership reads preserve
duplicates, verify the root contains itself and fail on missing required nodes.
The caller must authorize roots against its captured refs before using membership
as a visibility proof. Version 2 was an unreleased derived format; its owner
rebuilds it rather than introducing a retired-format reader. Its carry rule caps the layer directory at 32 entries even for decreasing
append batch sizes; internal page directories remain bounded. `CommitGraphReader`
loads only the descriptor and dependency ranges, preserves duplicate query
positions and validates selected parent ordering/corrected generations. It does
not grant ref visibility. Complete maintenance loading verifies OID/record
agreement, layer hashes, positional digest and physical byte coverage.
The released manifest producer/consumer path retains version 1 and its exact
encoding: upstream v1.0.1 through v1.2.4 publish `Manifest::commit_graph_hash` from
CLI push and read it in the remote helper. Lazy capsule readers reject that
format; maintenance may rebuild the unreleased eager browse index into pages.
Path and graph readers share the authenticated page codec and an 8-MiB/256-page
cache; each cache belongs to one immutable captured layer directory.
`CommitGraphView` supplies the same positional queries to complete maintenance
graphs and immutable paged readers. Path-state opening binds the paged descriptor's
verified positional digest; queried records are checked through batched graph reads.
It does not hash the complete paged graph during foreground opening. The outer
capsule/ref-control capture remains a separate bounded-opening qualification gate.

`qualify_graph_build` measures paged graph construction on the ancestry-growth
fixtures: linear, eight branches, criss-cross merges and disconnected commits.
Run each shape at 1k/2k/4k/8k commits under `/usr/bin/time -l`; repeat samples to
compare process CPU, RSS, complete encoded index bytes and ancestry-node counts.
Its synthetic OIDs measure builder cost, not Git object integrity or publication.
Keep results separate from real-store maintenance and foreground qualification.
For example, after selecting the checkout's external Cargo target:

```sh
cargo build -p crab-metadata --example qualify_graph_build --release --locked
/usr/bin/time -l "$CARGO_TARGET_DIR/release/examples/qualify_graph_build" branches 8000
```

Path-state descriptor version 6 requires exact-name tree comparison semantics
and authenticated pages; retired descriptors are rejected.
Maintenance rebuilds completed indexes and resumable checkpoints through the
existing conditional publication path. Record, node, component and child pages
have bounded authenticated directories. Raw components are stored once per layer,
with checked component ordinals on edges. `PathStateReader` fetches only the pages
required by a point/batch query. Its maintenance scan emits nodes before dependent
edges and records, verifies complete layer hashes and physical page coverage, and
retains only the current layer's dictionary/audit plus the bounded page cache.
Only successful EOF proves a complete scan; callers must not publish readiness
after partial output or an error. Full-layer hashing followed by page traversal
adds maintenance I/O while avoiding complete layer/index buffering.
No release tag contains a path-state format; maintenance rebuilds
unshipped derived indexes rather than
reading retired versions. Validated index descriptors/layers expose only immutable
accessors, so checkpoint append reuses the verified prefix instead of rescanning
all retained edges. Changed graph ordinals still require a prefix binding check.
Uploads admit the descriptor plus every retained/new layer against the intended
reader's byte limit before writing any layer; capsule and manifest builders use
their existing `max_path_state_bytes` bound. An oversized construction returns a
structured size error and cannot publish checkpoint or completion readiness.
Path-state construction and in-memory validation need no storage feature;
encoded-layer decoding is private to storage loading and codec tests.
Current file lookup selects the protocol from the root alone: only an absent
v2 root permits v1 lookup. A missing v2 dependency remains an error, and failed
shared-session initialization can retry after that dependency is repaired.

Layered checkpoint visibility uses authenticated dictionary, ref-closure and
transition pages, shared by readers and recovery. Checkpoint pointers require
explicit format 6 (`CRBCKP06`); retired or missing formats are rejected. No
release tag contains this layered format; the released v1 protocol is unchanged.
Footer roots bind the visibility section, retained-object count and ordered
source-catalog digest. `LayeredVisibilityReader` loads only pages needed for
requested ref membership; dictionary retention never authorizes visibility.
Loaded source/member positions are checked against checkpoint-owned sources,
before newer frontier sources are appended. Explicit bulk materialization
verifies the complete section hash and canonical encoding against captured roots.
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
An already verified complete run can yield the same authenticated control
view, including detached visibility/catalog sections, from its resident bytes;
consumers need not reread its control suffix.

Run compaction preserves exact Git object-to-member admission across ref-only
runs. Their authenticated empty pack directory proves an empty contribution;
a pack-bearing run without admission still prevents a complete merged proof.
`CapsuleRun::compact` consumes ordered runs with a bounded total of at most 512
capsules and encodes/authenticates the final run once. Run level is the
`ceil(log2(count))` size class; the authenticated capsule count remains exact.
Mixed-level carries retain canonical capsule bytes and member ordinals without
encoding discarded intermediate runs; complete capsule verification remains
mandatory. The v2 cutover uses CRBRUN01 with footer version 3 and
root/history/ref-head versions 4/3/5; there is no reader fallback to earlier
run or pointer semantics.
Runs may contain byte-identical packs from different transactions. Their source
directories retain every physical member and ordinal; identical content is
deduplicated only by readers. Source validation and readers share the same
content comparison, rejecting conflicting range lengths/hashes, sidecars, Git
checksums, object counts or external delta bases without comparing offsets.

`CRBRUN01` compacted runs also concatenate copies of their Git indexes into an
authenticated lookup pool. Original capsules and canonical member ranges stay
unchanged for recovery, installation and repack. Control-only readers derive
pool ranges in member order with the original index hashes; full decoding checks
the pool and every copied index. Leaves do not duplicate indexes. The run's
authenticated control suffix covers its exact member-admission bytes together
with the footer. Complete control loading verifies the entire admission hash
and exact suffix boundary before exposing placement hints. Git control loading
authenticates the footer and reads member-count pages while keeping the OID
directory cold. Payload bytes and the lookup pool remain outside the
suffix. Detached large visibility/catalog sections retain their separate bounded
reads. The unshipped `CRBRUN04`/`CRBRUN05`/`CRBRUN06`/`CRBRUN07` formats are
rejected, not read through a compatibility path; readers and writers must cut
over together.
Run footer version 3 rejects its retired versions 1/2; admission remains version 2.
Exact admission uses authenticated OID-to-member pages and dense per-member
count pages. Their roots are committed by the footer; complete readers verify
both directories and canonical body bytes before exposing admission. Page kind
codes are explicit, so payload-only and storage-enabled producers share one wire
format. The layout retains 500-capsule rollups and pooled indexes. Repositories
containing retired runs must be republished into a fresh repository prefix;
no automatic migration is provided. `load_capsule_run_git_control` preserves
complete reference controls and verifies counts against the pack directory.
Its paged admission reader returns authenticated misses and preserves request
order and duplicates; cached and fetched pages share one encoded-work budget.
Complete inspection still scans both admission trees. Complete visibility and
catalog decoding and source/ref capture remain separate opening costs.

Visibility delta version 3 replaces flat JSON OID lists with authenticated ref
headers and separate added/removed OID trees. Ref directories and object sets
page their internal routing. Empty sets have no root; shared immutable sets may
reuse pages. Complete delta decoding checks body identity, both sets, edit
invariants and canonical bytes. The existing visibility proof byte/ref/object
bounds also admit this encoding and prevent repeated roots from expanding into
unbounded logical work during complete inspection.

Run footers commit each visibility descriptor, and full control readers compare
it against the capsule section. Visibility OID tables are always detached from
the footer. `CapsuleRunControl::visibility_reader` opens authenticated batched
changes for one captured capsule, preserving duplicates and input order. A
missing ref, an unchanged OID and a missing/corrupt page have distinct results.
Replacement edits must admit their new tip; queried OIDs cannot belong to both
change sets. These changes alone never authorize a ref or object want.
The capsule delta's retired JSON shape has no reader or serialization alias;
retired run prefixes require republishing. Released v1 visibility index and
manifest/catalog formats are unchanged. Git control capture retains explicit
paged evidence; `visibility_delta` returns an error for unloaded data rather
than treating it as absence. Scalar header reads and actual-set inspection share
the pager's encoded-work admission. Complete materialization verifies the captured
section hash and descriptor, checks canonical delta bytes, and retains immutable
shared results. `crab-read` owns lazy closure reconciliation; complete metadata
capture remains available for maintenance and publication.

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

### Paged path attribution

Unreleased path-state descriptor version 6 authenticates bounded record, node,
component and child pages inside each immutable layer object. Canonical full
loads validate the complete physical closure; `PathStateReader` resolves selected
paths through verified ranges and a bounded parsed-page cache. Graph and ordinal
bindings are retained. GC retains the layer object and therefore every embedded
page. See the [format and qualification contract](../../crab/docs/architecture/indexed-remote-reads.md#paged-path-state-format).

Path-state nodes retain deletion/type-reset ordinals and historical descendants.
A reset masks older visible values; it does not mutate prior commit roots.
`last_change` locates candidate changes for absent paths as well as visible ones.
A covering reset may nominate a never-existing descendant; raw tree comparison
must decide whether that candidate actually changed the requested path. Full
loads, resumable construction, compaction and maintenance scans retain markers.
The retired unreleased version 5 is rebuilt by existing maintenance, never read
through an alternate codec.

Run footer version 4 adds an embedded `CRBRCG01` catalog index. Its four paged
key directories retain exact capsule occurrences and group equal immutable
values by a semantic commitment that excludes physical page offsets. Full run
verification rebuilds the index from nested capsules, so re-signed membership
changes cannot invent catalog authority. Footer-only reads admit the index
region and source spans without fetching its body. Captured run sources feed
batched file, shard, xorb and reverse-chunk lookup through the checkpoint
reader's shared cache and aggregate budget. Exact selected capsule ordering
controls file precedence; unselected occurrences cannot introduce conflicts.
Nested recipes retain authenticated page references into their original catalog
sections and must match the aggregate's logical-value commitment.
Git controls retain catalog section commitments and captured origins rather
than loading catalog bytes. Full maintenance inspection materializes those
captured sections explicitly. Run footers contain catalog roots, never inline
catalog bodies. This reader wiring is under qualification; it does not establish
the complete history-scaling, lifecycle or 10 GiB acceptance gates.
The affected run format is absent from the audited release tags; unshipped
version 3 is rejected rather than retained through a fallback.
