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

### Capsule roots and catalogs

Capsule protocol v2 uses one compacted root plus independently mutable ref
heads and immutable capsule runs. Complete capsule reads share storage's bounded
immutable transfer policy, including concurrent ranges for large seed capsules.
Full decoding still materializes the body and validates every pointer commitment.

Run construction reuses verified immutable capsule descriptors. Compaction checks
byte-for-byte equality when moving capsules into its new buffer, releases the old
buffers, and hashes the completed run once. Downloaded runs still pass full capsule,
section, pooled index, catalog, and admission verification; the wire format is unchanged.

`capsule_protocol::CapsuleChunkIndex` reads paged global advisory chunk heads
and exact placement IDs without opening the released manifest receipt database.
`write_capsule_chunk_candidates` conditionally publishes verified immutable pages
after ref visibility, under caller-held GC writer admission. Readers need no
write grant; hints still require current source-catalog and canonical-origin
proof. `CapsuleChunkIndex::objects` authenticates the physical closure for GC;
it does not make hints authoritative payload roots.

Pointer catalogs keep shard/xorb payloads
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

Checkpoint format 9 (`CRBCKP09`) commits to a small catalog descriptor and
immutable content-addressed pages under `v2/catalogs/<partition>/<hash>`.
Descriptor version 2 (`CRBCAT02`) authenticates the roots of file bindings,
shards, xorbs and reverse chunk placements. Internal directories and nested
recipes use independent pages (`CRABOP01`); unchanged subtrees retain their
identities across checkpoint publication. New pages and the descriptor are
verified before checkpoint/history writes and root CAS. Ref-only publication
and physical repacking reuse the existing commitment without catalog object I/O.

A catalog delta resolves its selected dependency closure and rewrites affected
paths, preserving ordered file bindings and immutable descriptor conflicts.

`merge_checkpoint_catalogs` unions durable roots without retaining logical
recipe maps. Complete input proofs and merge batches share one encoded-page
budget. It retains newer file bindings, checks immutable descriptors and file
sizes, chooses canonical reverse placements, and verifies immutable writes.

Callers hold GC admission through publication and separately prove external
payloads and final file bytes. Unchanged subtrees retain their identities.

### Catalog inspection and lookup

Readers share an aggregate page-byte budget across checkpoint and packed-run
sources. Logical expansion has a separate allowance of four times the encoded ceiling: shared
physical pages cannot authorize unbounded recipe/result copies. Cold and warm
reads consume admission; decoded caches have independent resident limits.

Physical catalog-object inspection visits authenticated pages once and spills
bounded relation batches into a private local workspace. Sorted reductions check
file/shard/xorb dependencies and the lowest-xorb/first-occurrence reverse mapping
without random origin joins or a complete logical catalog in memory. Shared
positional pages retain their earliest canonical owner; inconsistent references
still fail.

The existing temporary-directory setting selects the workspace parent;
its live disk ceiling is four times the caller's encoded-byte ceiling. Workers
retain file ownership through cancellation and are drained before normal/error
cleanup. No authoritative or cache object is written by inspection.

Explicit full-catalog callers still materialize it; converting remaining maintenance
consumers is separate work.

`inspect_pointer_catalog` reduces a complete captured checkpoint and selected
deltas through private spill, resolves file-binding precedence and validates the
final dependency closure before returning `PointerCatalogInspection`. GC collects
shard identities from this owner; replica readiness streams full shard/xorb
descriptors without a joined catalog in memory.

The cursor retains the normalized
run until `finish` drains workers and removes it, preserving the operation's primary
error if cleanup also fails. A cancelled pending read can resume at the same
object.

Complete cardinalities remain bound to the proof even after cursor
consumption; sampled physical checks cannot stand in for these counts. The
cursor also retains the checked encoded-byte total of distinct shard/xorb
bodies. This is metadata accounting; physical verification remains required.

Physical page identities, spill handles and the largest recipe still
consume memory; each packed delta retains its format-bounded decoder. Logical
reduction has a live disk ceiling of sixteen times the admitted catalog limit;
normal lookup and materialization budgets remain unchanged.

`PointerCatalogInspection::group_by_shard` consumes the complete verified capture,
including rows already visited by its object cursor. It reorders only private
spill blocks: xorbs first, then each shard and its final file bindings. The same
disk ceiling and cleanup owner apply; the original run retires before the grouped
cursor returns. Ordinary GC/readiness consumers do not pay this extra reduction.

Fsck consumes bounded file-binding batches while retaining one shard body and its
xorb metadata, rather than a complete file map or a scan of every file per shard.

GC traverses catalog pages from current, historical and retained browse roots.
Protected keys and authenticated closure traversal are tracked separately.

Complete checkpoint visibility proofs are authenticated once and shared across
immutable checkpoint clones. Building a checkpoint retains its validated proof;
control-only readers keep the visibility body cold.
Checkpoint footer history projects its recent suffix from validated ordinals
without rebuilding the complete visibility index. Hints consume only the bytes
remaining after required control metadata within the 8 MiB footer bound. Large
transitions remain complete in authenticated visibility pages; missing hints use
the existing verified Git traversal. Required metadata that cannot fit returns
`CheckpointFooterSize` so the publication owner can consolidate physical sources.

Retired unshipped checkpoint formats through 8 are rejected without fallback.
The six published release tags do not contain this capsule codec.
This format is under qualification; earlier format-7/8 runtime receipts do not
qualify format 9 or establish hosted-S3 performance.

Retained browse record format 2 carries the complete checkpoint pointer and
catalog identity; loading authenticates the footer and rejects omitted or
mismatched catalog/source dependencies.

`LayeredCheckpoint::pointer_catalog_lookup` captures these roots;
`PointerCatalogLookup` batches file/chunk/xorb keys through one bounded page
cache and byte budget. Its `dependencies` query selects files plus explicit
shard/xorb identities in one aggregate budget, including complete dependency
closure. Missing candidate identities remain misses; missing referenced
artifacts and conflicting visible descriptors fail. Full inspection remains
an explicit maintenance operation.

`SharedFileIndexLookup::for_pointer_catalog`
accepts captured lookup roots and resolves requested files with their authenticated
shard/xorb closure. Point and batch calls share the page cache and retain the
session read guard through I/O, so close waits for active reads. Missing catalog
pages remain errors rather than reopening the mutable file-index database.

A session caches at most 4,096 verified catalog bindings or authenticated misses,
using the same result-cache owner as canonical shard lookup. Cold queries still
validate the full selected dependency closure before publishing cache entries;
errors are never cached as absence. Cached inputs retain the original admission
charge.

Catalog cache locks cover memory work only, so unrelated warm lookups can finish
while a cold origin read is paused. Retention counts distinct file identities;
duplicate occurrences do not consume additional entries or prevent caching.

The current-state opener also captures authenticated catalog roots and leaves
catalog bodies cold. Explicitly pinned callers pass their existing capture, while
current-state sessions bind once to the first root they open. Catalog and Git
views share stable ref capture, ref-frontier selection, visibility reconciliation
and transaction ordering; neither path invents a separate precedence policy.

All six upstream release tags audited for format 8 omit the capsule protocol
codec. Earlier formats, including unshipped checkpoint format 7 and browse
retention record format 1, are rejected explicitly. Unreleased fixtures must be
recreated; no fallback reader or automatic authoritative conversion is provided.

Checkpoint and selected run catalog roots share one lookup budget and page cache.
CLI and shared capsule-publication preparation use these captured queries.
Protected receive selects from separately authorized source/filtered captures.
Captured revision, remote-mount and current-state hydration use the shared paged
lookup. Control capture and selected closure queries are measured separately;
complete opening and history-amplification qualification remain required.

Ref deletion removes its live visibility closure even when the capsule has no
visibility payload. Retained dictionary objects cannot supply a new ref's base;
complete and paged reconciliation enforce the same deletion boundary.

### External pointer catalogs

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
still requires each delta's dependencies to exist in its base.

Encoded catalogs are limited to 2 GiB and descriptors to 4 KiB.
The retired monolithic JSON encoding was absent from all 19 audited release tags
and is rejected.

Shared file lookup uses paged capture for pinned and current-state sessions.
Complete catalog inspection remains explicit; consumer, historical amplification
and lifecycle qualification must not be inferred from the page representation.

### Browse indexes and retained roots

`v2/browse-indexes` is a bounded, mutable derived record, not ref authority.
Its version 3 record binds a durable immutable commit graph, optional complete
physical object lookup and optional complete path attribution to the exact
capsule state, including visible per-ref transactions. Physical lookup readiness
requires complete verified pack coverage before publication; path completion
retains that binding. Graph publication does not wait for path construction.

The prior versions were never reachable from a release tag; maintenance rebuilds this derived
record in place, without a legacy reader. Readers ignore stale
records; absent indexes remain a readiness state.

GC retains dependencies of the currently
published readiness record even when newer refs make it unusable for new reads.
Storage retention does not grant inventory membership or ref visibility.

The separate `v2/browse-retention` CAS head owns a bounded chain of authenticated
`v2/browse-roots/` captures. Maintenance records original checkpoint, Git source and all pending capsule-run
identities before advertising replacement readiness. Ref-only visibility/catalog
runs remain dependencies even when they contain no Git pack members. GC follows every
retained capture's graph, path-state, physical pages and original source bodies;
foreground reads still load only the current bounded readiness record. An
unchanged capture does not append a node. Missing/corrupt lifetime dependencies
stop GC before deletion. A valid readiness record without lifetime ownership
requires background browse maintenance before GC; there is no scan fallback.

Full capsule fsck also authenticates the retained chain, decodes complete
graph/path-state indexes, and verifies physical-index and original source bytes,
including superseded ref-only runs. Shared captures stream each source body once
during this phase. Fsck holds the verified lifetime/readiness capture through
payload verification and the final snapshot check. Revalidation reads only bounded
control records; valid movement requires a retry and corruption fails validation.
Mirror pointer-recipe probes do not initialize this full maintenance workload.

Explicit history pruning trims superseded captures below the oldest retained
checkpoint generation only after the authoritative history CAS succeeds. Current
readiness and the newest lifetime capture remain rooted. A failed cleanup leaves
extra retention and is retried even if the history chain already has the requested
length. Pruning is the boundary that can invalidate captures from discarded
generations; ordinary rebuilding, checkpointing and GC do not expire readers.
The new lifetime format is version 1; no release tag contains capsule browse
readiness, and its existing version 3 encoding remains unchanged.

Compaction also retains replaced capsule sources independently of browse
readiness. Before publishing a replacement frontier, the ref owner CAS-publishes
a per-ref head under `v2/history/source-heads/` and authenticated source-pointer
segments under `v2/history/source-roots/`. Foreground catalog sessions stay
read-only and keep their original source ranges. GC discovers these roots in its
existing history enumeration; fsck authenticates their original run proofs.

Fsck retains the decoded heads' CAS tokens and the pruning floor through later
data and Git checks. It rechecks mutable authority without decoding history again;
concurrent movement requests a retry, while invalid control bytes fail validation.

Explicit history pruning advances a persistent generation floor after history
CAS, trims older segments, and prevents expired publishers from restoring them.
These new lifetime records do not change capsule, checkpoint, or catalog formats.
They cover sources retired by updated publishers; backfill/rollout qualification
for previously compacted unreleased repositories remains a release gate.

### Commit graph construction

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
local page hashes alone cannot certify ancestry.

Point membership reads preserve
duplicates, verify the root contains itself and fail on missing required nodes.
The caller must authorize roots against its captured refs before using membership
as a visibility proof. Version 2 was an unreleased derived format; its owner
rebuilds it rather than introducing a retired-format reader.

Its carry rule caps the layer directory at 32 entries even for decreasing
append batch sizes; internal page directories remain bounded.

`CommitGraphReader` loads only the descriptor and dependency ranges, preserves duplicate query
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

### Path-state pages

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
reading retired versions.

Validated index descriptors/layers expose only immutable
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

### Checkpoint visibility and history

Layered checkpoint visibility uses authenticated dictionary, ref-closure and
transition pages, shared by readers and recovery. Checkpoint pointers require
explicit format 6 (`CRBCKP06`); retired or missing formats are rejected. No
release tag contains this layered format; the released v1 protocol is unchanged.

Footer roots bind the visibility section, retained-object count and ordered
source-catalog digest. `LayeredVisibilityReader` loads only pages needed for
requested ref membership and physical pack-member selection; dictionary retention
never authorizes visibility. Member selection preserves request order and misses,
checks source bounds, and shares the authenticated page cache.
Loaded source/member positions are checked against checkpoint-owned sources,
before newer frontier sources are appended. Explicit bulk materialization
verifies the complete section hash and canonical encoding against captured roots.

Paged control replay batches checkpoint dictionary membership for its captured
additions before applying transactions. Temporary positive and negative results
avoid repeatedly fetching the same dictionary pages across a long frontier.
They describe initial retention only: ref ordering, prior closures, removals,
new-tip membership and the exact retained-object limit remain checked when each
control is applied. The captured replay releases these temporary results afterward.

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

### Compacted runs and admission

Run compaction preserves exact Git object-to-member admission across ref-only
runs. Their authenticated empty pack directory proves an empty contribution;
a pack-bearing run without admission still prevents a complete merged proof.

`CapsuleRun::compact` consumes ordered runs with a bounded total of at most 512
capsules and encodes/authenticates the final run once. Run level is the
`ceil(log2(count))` size class; the authenticated capsule count remains exact.
Mixed-level carries retain canonical capsule bytes and member ordinals without
encoding discarded intermediate runs; complete capsule verification remains
mandatory.

The v2 cutover uses CRBRUN01 with footer version 3 and
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
the pool and every copied index. Leaves do not duplicate indexes.

The run's authenticated control suffix covers its exact member-admission bytes together
with the footer. Complete control loading verifies the entire admission hash
and exact suffix boundary before exposing placement hints.

Git control loading authenticates the footer and reads member-count pages while keeping the OID
directory cold. Payload bytes and the lookup pool remain outside the
suffix.

Detached large visibility/catalog sections retain their separate bounded
reads. The unshipped `CRBRUN04`/`CRBRUN05`/`CRBRUN06`/`CRBRUN07` formats are
rejected, not read through a compatibility path; readers and writers must cut
over together.

Run footer version 3 rejects its retired versions 1/2; admission remains version 2.
Exact admission uses authenticated OID-to-member pages and dense per-member
count pages. Their roots are committed by the footer; complete readers verify
both directories and canonical body bytes before exposing admission. Page kind
codes are explicit, so payload-only and storage-enabled producers share one wire
format.

The layout retains 500-capsule rollups and pooled indexes. Repositories
containing retired runs must be republished into a fresh repository prefix;
no automatic migration is provided.

`load_capsule_run_git_control` preserves
complete reference controls and verifies counts against the pack directory.
Its paged admission reader returns authenticated misses and preserves request
order and duplicates; cached and fetched pages share one encoded-work budget.

Complete inspection still scans both admission trees. Complete visibility and
catalog decoding and source/ref capture remain separate opening costs.

### Visibility deltas

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
manifest/catalog formats are unchanged.

Git control capture retains explicit
paged evidence; `visibility_delta` returns an error for unloaded data rather
than treating it as absence.

Scalar header reads and actual-set inspection share
the pager's encoded-work admission. Complete materialization verifies the captured
section hash and descriptor, checks canonical delta bytes, and retains immutable
shared results.

`crab-read` owns lazy closure reconciliation; complete metadata
capture remains available for maintenance and publication.

### Payload modules and features

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
remain. Concurrent close calls also wait for reader cleanup.

Await close to completion; dropping the owner or close future cannot perform asynchronous
cleanup. Scoped stores retain write-free canonical reads.

### Snapshot-bound lookup

Integrity callers use `FileIndexLookupSession::from_snapshot` with an already
captured `RepositorySnapshot` and its scoped storage layout. This constructor
does not open SlateDB, refresh metadata, or write reader checkpoints.

Lookup uses canonical shards, including committed journal additions; duplicate recipes
select the smallest shard hash deterministically.

The caller still owns freshness revalidation and protection against concurrent GC. This path scans
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

Locator maintenance bounds the shared L0 frontier and compaction fan-in at 16.
The coordinator, worker and result commit poll every 500 ms while maintenance
is enabled. This prevents frequent checkpoint flushes from starving sorted-run
compaction or waiting on the dependency's five-second coordinator default.
Publication enables maintenance under measured L0 pressure; coverage-only and
reader-repair writers keep their existing maintenance-free behavior.

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
publication base before accepting a write.

The composing request must still own
an overall deadline and admission for the other receive stages. A timed-out
lookup may finish its current bounded parser in the background, retaining its
permit until completion; the caller must await session close before releasing
its operation resources.

Pointer content proofs have their own four-operation
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

### Paged run catalogs and visibility

Run footer version 5 retains the embedded `CRBRCG01` catalog index introduced in
version 4. Its four paged
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

Compacted runs also concatenate copies of their visibility sections after the
Git index pool. The footer binds the pool's span and digest; full verification
checks each copy against its original capsule section.

Logical controls share one cache of decoded pages and 64 KiB proof blocks, bounded to 8 MiB and 256
entries per physical run. Read-ahead stays inside the visibility pool, never
crossing Git payloads, and charges the complete fetched blocks on cold and warm
queries. Every requested page still authenticates against its capsule's roots.

Expected-old checks, captured prior closures, canonical transaction ordering and
the distinct-object proof limit remain unchanged. This reduces per-edit network
round trips; logical replay and distinct historical-object growth remain open
qualification gates.

The current capsule format is absent from the six published release tags and
19 known local tag objects audited on 2026-10-15. The published `v1.2.4` object
differs from the local tag; both were inspected without replacing that tag.
Unshipped run footer versions 1 through 4 are rejected, with no fallback reader.
