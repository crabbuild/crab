# Protocol v2 Xorb and Shard Integration

## Document metadata

| Field | Value |
| --- | --- |
| Project | Crab |
| Scope | Pointer push, clone, fetch, hydrate, mount, recovery, and GC |
| Status | Protocol core and major terminal/server paths implemented; complete v1 product parity and current-format production qualification remain open |
| Priority | Correctness, large-file efficiency, then request latency and throughput |
| Companion | [Capsule Publication Protocol](capsule-publication-protocol.md), [Push Pipeline Deep Dive](push.md), [Canonical Object Storage Layout V1](../architecture/object-storage-layout.md) |

## 1. Decision

Protocol v2 retains xorbs and shards as independent, content-addressed object
store objects. It does not embed their payload bytes in publication capsules.

The design combines the strongest responsibilities of both protocols:

- v2 owns foreground transaction publication through independently mutable
  per-ref heads; one ref-head CAS commits a single-ref push, while a unique
  transaction-record CAS commits a prepared multi-ref push and an immutable
  marker records its durable completion;
- xorbs retain chunk aggregation, compression, immutable identity, independent
  caching, storage tiering, repair, and cross-file reuse;
- shards retain complete file reconstruction terms;
- capsules authenticate the ref transaction and the exact external dependency
  set, but remain bounded metadata and Git containers;
- the repository root is cold control-plane state for checkpoints, HEAD,
  capabilities, and GC fencing rather than a foreground push bottleneck;
- checkpoints compact per-ref histories and derived catalogs without rewriting
  live xorb payloads;
- the bucket ref registry protects shared objects before a root can publish a
  new reference to them.

The implementation uses `CatalogDelta` sections for authenticated dependency
metadata while leaving `FileData` and `FileRecipes` unused. Pointer payloads
remain in canonical external xorbs and shards. The primary surfaces are
`crates/crab-metadata/src/capsule_protocol/`,
`crab/src/git/capsule_push.rs`, and the shared file-index lookup.

## 2. Goals

The implementation MUST:

1. Preserve byte-identical reconstruction or return an error.
2. Make every xorb and shard required by a new ref durable before that ref is
   visible.
3. Publish Git refs, pointer visibility, shard recipes, and xorb dependencies
   as one per-ref transaction, atomically across all edited refs.
4. Preserve stable xorb and shard content identities across files and
   repositories.
5. Avoid foreground existence probes for dependencies already proven by the
   pinned base generation.
6. Keep a pointer-free small push on the four-request qualified or
   five-request readback path, including post-commit ref-epoch confirmation.
7. Scale pointer-push requests with newly created immutable payload objects,
   not files, chunks, recipes, metadata rows, or total repository history.
8. Support independent xorb caching, range-free hydration, storage-class
   transitions, replication, and repair.
9. Keep normal GC safe with concurrent pushes and conservative failure
   recovery.
10. Bound catalog traversal through checkpoints while leaving xorb bytes in
    their canonical objects.

## 3. Non-goals

This design does not promise a constant request count for an arbitrarily large
file push. One independently stored xorb requires at least one create request,
and multipart transfer adds one request per part plus lifecycle operations.

It also does not:

- treat an ETag, Bloom filter, cache hit, or unverified index row as durability
  proof;
- restore v1's per-chunk `HEAD`, per-record publication, ref-lock, journal, or
  SlateDB foreground request fan-out;
- make a mutable global reference count part of hydration correctness;
- publish pointer refs before the external dependency closure is complete;
- require clone or fetch to download large-file payloads before checkout.

## 4. Invariants

1. **Durable before visible.** Ref-head publication occurs only after every
   newly required xorb, shard, Git object, capsule, and GC protection record is
   durable.
2. **Complete recipe.** A committed shard covers every byte of each file
   version it declares, in order, with no gap or overlap.
3. **Authenticated closure.** A capsule commits to every shard introduced by
   the transaction and every xorb required by those shards that is not already
   proven by its pinned base.
4. **Authoritative omission.** A writer may omit a payload only when the pinned
   base root and its verified catalogs prove that exact content identity
   reachable, or after it independently verifies and protects the shared
   canonical object.
5. **Snapshot reads.** A read captures the root, lists complete ref-head object
   metadata before and after loading the heads, and retries if any key or
   provider version changed. It resolves each referenced activation record
   exactly once. A committed record selects every prepared state for that
   activation; a preparing or aborted record selects every predecessor, so a
   multi-ref transaction and its later successors are never partially visible.
6. **Independent verification.** Readers verify capsule, shard, xorb, chunk,
   file, and Git identities at their respective boundaries.
7. **GC protection precedes publication.** Bucket-global objects are registered
   conservatively before a ref head can reference them. Reuse outside
   the pinned base also holds a GC publication guard across verification and
   ref publication.
8. **Conservative leaks are safe.** Failed pushes may leave immutable objects
   and protection records, but never a visible dangling pointer.

## 5. Storage model

Paths are relative to the existing validated global and repository prefixes:

```text
{global_prefix}/
├── xorbs/{first-two-hex}/{xorb-blake3}
├── shards/{first-two-hex}/{shard-blake3}
└── ref-registry/
    ├── records/{repo-fanout}/{repo-blake3}.json
    └── shard-roots/{repo-blake3}/{partition}.json

{repo_prefix}/v2/
├── root
├── refs/heads/{hex-encoded-ref}.json
├── transactions/
│   ├── records/{activation-id}.json
│   └── committed/{activation-id}.json
├── plans/{plan-id}/
│   ├── intent.json
│   └── terminal.json
├── capsules/{first-two-hex}/{capsule-blake3}
└── checkpoints/{first-two-hex}/{checkpoint-blake3}
```

Xorbs and shards preserve the canonical v1 keys. They are immutable and use
create-only writes. Capsules and checkpoints are repository-local. Each ref
head is the mutable authority for only that ref. The root changes only for
bounded checkpoint or maintenance work, so pushes to disjoint refs never CAS
one shared root object.

The repository's ref-registry record is stable discovery metadata for bucket
GC. It identifies the repository, layout version, and canonical v2 root key;
it does not advertise Git refs or authorize reads. Initialization creates or
updates this record before publishing generation zero. Pointer pushes append
candidate shard closures to versioned, partitioned registry records. Repository
deletion tombstones discovery first and retains roots and immutable data for
the configured recovery period.

### 5.1 Logical and physical identity

A logical xorb identity is the Merkle hash of its ordered chunk hash and size
sequence. It is intentionally independent of compression encoding. A logical
shard identity is the BLAKE3 digest of its canonical serialized recipe bytes.
Neither logical hash depends on repository, capsule, checkpoint, storage
class, or replica.

The catalog records physical location separately. A later repair or replica
operation may add a verified physical copy without changing logical identity
or file recipes.

### 5.2 Xorb contract

An xorb remains a bounded compressed aggregate with:

- canonical format and codec version;
- ordered chunk entries;
- chunk hash, uncompressed size, and encoded range for each entry;
- aggregate encoded size and content hash;
- enough framing to reject truncation, extension, reordering, and corruption.

Two valid encodings can therefore occupy the same logical xorb identity. A
create conflict is not accepted from its key alone: the writer bounded-reads
the stored body, verifies the logical xorb hash, encoded-payload digest, every
decompressed chunk hash, and the expected placements, then records the stored
encoding's actual size and body digest in the catalog. A mismatch fails closed.

No repository ref points directly to an unverified xorb location. A shard and
the pinned catalog mediate the reference.

### 5.3 Shard contract

A shard contains:

- each file hash and declared byte size;
- an ordered, complete sequence of xorb chunk ranges;
- the exact reconstructed byte contribution of every term;
- the referenced xorb set or a digest of its canonical closure;
- format version and content hash.

Finalization rejects missing chunks, duplicate coverage, gaps, overlaps,
incorrect total size, and references to xorbs absent from the candidate
dependency closure.

## 6. Capsule and checkpoint contracts

### 6.1 Thin publication capsule

A pointer-aware capsule contains no xorb or shard payload bytes. It contains:

```text
ref_transaction
base_root_digest
git_pack_descriptors
required_shards[]     // hash, encoded size, xorb-closure digest
required_xorbs[]      // hash, encoded size, format version
file_catalog_delta[]  // file hash and size -> shard hash
xorb_catalog_delta[]  // xorb identity -> canonical global location
visibility_delta
transaction_digest
```

The transaction digest covers the ordered canonical encoding of every field.
Descriptor arrays are sorted and deduplicated. Conflicting descriptors for one
content identity fail locally before any remote mutation.

`required_shards` and `required_xorbs` include newly uploaded objects and
externally reused objects that needed fresh verification and GC protection.
Dependencies already reachable from the exact base root need not be repeated
as payload, but the resulting file catalog must still resolve their identities
through the combined view.

### 6.2 Ref heads and root

Each ref head contains committed state and, only for a multi-ref transaction,
one prepared state. A state binds the ref OID, peeled OID, newest transaction,
and a bounded frontier of immutable leaf capsules. Foreground publication does
not read or rewrite older capsules. Background checkpoint maintenance folds a
complete authenticated view after 32 visible capsules; the next writer drops
the exact checkpointed prefix and preserves any concurrently published suffix.
The 64-entry hard bound leaves maintenance headroom without making an
unbounded read contract.

The v2 root contains the compacted ref baseline, exact per-ref checkpoint
positions, generation, parent digest, checkpoint, capabilities, GC fence, and
root digest. It does not inline file, shard, or xorb catalogs and is not changed
by ordinary pushes.

Pointer capability is advertised only when the root's checkpoint and frontier
jointly provide complete file and xorb catalogs for that generation.

### 6.3 Multi-ref atomicity without a hot root

Every multi-ref publication attempt has a fresh activation ID, even when it
retries the same content transaction. Its mutable transaction record begins in
`preparing`. Each edited ref head is conditionally replaced with a two-version
state containing the predecessor, successor, and activation ID. The writer
then arbitrates `preparing -> committed` with one conditional record update and
creates the matching immutable committed marker. The record CAS is the
linearization point. No two attempts share either coordination object unless
they edit the same ref heads.

A competing writer that encounters `preparing` may conditionally change that
record to `aborted` and advance from the predecessor. Commit and abort cannot
both win the same record CAS. A committed record missing its marker remains
visible and recoverable: a writer or plan resolver recreates the exact
immutable marker. A unique activation ID prevents an aborted retry from
reviving prepared heads from an earlier attempt.

Readers list ref-head object metadata, fetch and authenticate those exact
heads, resolve each distinct activation record referenced by a prepared head
once, then list ref-head metadata again. A changed key, ETag, version, size, or
modification time restarts the bounded capture. This double collection is
required because a later single-ref successor may replace one prepared head;
a one-sided snapshot could otherwise combine that successor with another
ref's predecessor. The activation record is read once for all participating
heads, so its single status selects all-old or all-new without a global marker
scan.

An explicit push does not enumerate the repository. It reads each destination
head twice by its deterministic key, resolves only activation records named by
those heads, and authenticates only their bounded run frontiers. Matching
before/after head bodies and provider versions gives the same stable-snapshot
property without work proportional to unrelated branches. If a selected head
belongs to a multi-ref transaction, its capsule carries the complete atomic
edit and the shared activation record still selects all-old or all-new.

This removes the root hot spot for hundreds of branches. Updates to existing
distinct refs share no mutable key. Same-ref writers still serialize at that
ref head, as correctness requires. Ref creation and deletion conservatively
share a hashed gate for the first component below `refs/<kind>/`; this protects
Git directory/file conflicts, while steady-state updates bypass the gate.

Per-ref authority removes write contention, but it does not by itself make a
complete Git ref advertisement constant-cost. The exact full-view reader still
lists the head namespace and authenticates each returned head. A long-lived
remote-helper session reuses that view; many fresh processes over hundreds of
branches can create linear read amplification even though their writes are
independent. A request-minimal advertisement layer may add immutable,
hash-partitioned ref-index snapshots and targeted `ls-refs`, but such an index
is derived acceleration only: a push still validates and conditionally writes
the destination's authoritative per-ref head, and a stale index can never
authorize publication.

### 6.4 Checkpoint

A checkpoint compacts metadata, not large-file payloads. It contains:

- the complete Git catalog and pack evidence;
- complete file-hash-to-shard mappings;
- the complete live shard set and its authenticated xorb closure;
- complete xorb identity and location mappings;
- visibility state;
- covered root generation and digest.

Checkpoint publication does not rewrite canonical xorbs or shards. Once the
checkpoint is visible, older capsules may be collected when retained-root and
history policy no longer require them. Shards and xorbs remain live while the
checkpoint catalog reaches them.

## 7. Pointer push

### 7.1 Pin and validate the base

An explicit push opens and verifies `v2/root`, directly captures only its
destination heads, and resolves each activation referenced by those heads
once. Pointer preparation then expands to the complete visible checkpoint and
ref frontiers because cross-ref xorb reuse and GC safety require a complete
snapshot-pinned file, shard, and xorb catalog. Pointer-free pushes remain on
the targeted path.

The writer validates expected-old refs, fast-forward policy, pointer
visibility, catalog completeness, and the root's advertised pointer
capability. Final ref-head CAS detects same-ref staleness without rejecting a
concurrent push to another branch.

### 7.2 Discover pointers and staged content

The writer walks the outgoing Git object closure relative to the pinned base
and parses candidate pointer blobs. For each distinct `(file_hash, size)` it:

1. loads the complete staged chunk sequence;
2. validates chunk hashes and total file size;
3. reuses an existing staged preparation when its digest and lease remain
   valid;
4. groups new chunks into bounded xorbs;
5. builds a complete shard recipe;
6. verifies reconstruction locally before remote mutation.

Multiple paths and refs sharing a file version use one file-catalog entry.
Multiple files sharing chunks reuse the same candidate xorb.

### 7.3 Classify dependencies

Every candidate xorb is classified into one of three states:

| State | Required proof | Action |
| --- | --- | --- |
| Reachable from pinned base | Verified checkpoint/frontier catalog | Reuse without remote request |
| Newly created by this push | Locally verified canonical bytes | Upload create-only |
| Present outside pinned base | GC publication guard, full origin verification, and registry protection | Reuse through the guarded path or upload a safe new placement |

A local cache or global dedup service may suggest the third state but cannot
establish it. If the canonical global key already exists, the writer verifies
its complete bytes while holding the GC publication guard before publishing a
new reference. A stored checksum may replace body transfer only when provider
qualification proves that the checksum is cryptographically bound to that
exact stored version.

### 7.4 Build immutable outputs

The writer builds and locally verifies:

- all missing canonical xorb objects;
- all new canonical shard objects;
- the standard Git pack and its index, reverse index, and locator evidence;
- one thin capsule binding the Git ref transaction to the external dependency
  closure;
- one monotonic registry candidate containing the transaction's shard roots.

The capsule cannot be constructed successfully unless every pointer in the Git
transaction resolves through the candidate file catalog.

### 7.5 Upload and protect

The current writer conservatively acquires the existing global and repository
GC publication guard for every pointer-bearing push. It cannot know before a
create-only write whether a candidate xorb is new or an old globally shared
object, so narrowing the guard without another request would permit a sweep
race. The guard remains held through immutable verification, registry union,
and ref publication. Git-only pushes do not acquire it.

After hashes and any required guard are established, the writer starts these
independent operations with bounded concurrency:

- create missing xorbs;
- create new shards;
- create the capsule run;
- union the candidate shard roots into the repository's bucket ref-registry
  partition.

All immutable writes use provider-qualified cryptographic checksums when the
endpoint has passed qualification. Other endpoints require full readback.
ETags are version tokens, not content hashes.

The registry union is monotonic before ref publication. A failed push may
over-retain its candidate closure until registry compaction, but GC cannot
delete a candidate that a concurrent ref-head CAS is about to publish: new objects
are protected by age grace, base-reachable objects by the pinned old root, and
externally reused objects by the publication guard.

### 7.6 Publish

Only after all immutable writes, verification, and registry protection succeed
does the writer update ref authority:

- a single-ref push conditionally replaces only that ref head; its CAS is the
  linearization point;
- a multi-ref push creates a unique preparing record, conditionally prepares
  every edited head, wins the record's commit-vs-abort CAS, then creates the
  matching immutable committed marker; the record CAS is the visibility point;
- prepared heads retain their predecessor, and readers double-collect complete
  head metadata around head and activation reads, so overlapping commits force
  a retry instead of a partial view;
- same-ref CAS failure rejects stale expected-old state;
- disjoint-ref pushes share no mutable publication object;
- an uncertain head, record, or marker response is reconciled against the exact
  canonical body, activation ID, and transaction identity.

### 7.7 Cleanup

Success retires local staging ownership only after the committed ref authority
is observed. Failure retains staged content for retry. Remote immutable objects
and monotonic registry entries are not synchronously deleted.

## 8. Clone, fetch, checkout, and hydrate

### 8.1 Clone and fetch

Git clone and fetch transfer standard Git objects, including small Crab pointer
blobs. They do not eagerly transfer pointed-to file content unless an explicit
prefetch policy requests it.

The Git path remains the protocol-v2 checkpoint and capsule-pack path. Before
advertising pointer capability, the reader also validates that the pinned file
and xorb catalogs cover every visible pointer dependency.

### 8.2 Checkout

Checkout writes pointer blobs or delegates selected paths to hydrate, smudge,
or the VFS according to existing product policy. It never mistakes a pointer
for an available file merely because its catalog entry exists.

### 8.3 Hydrate and smudge

For each distinct file version, the reader:

1. pins and verifies one root generation;
2. resolves `(file_hash, size)` to one shard hash;
3. fetches the shard from immutable cache or canonical storage;
4. validates the shard hash, format, coverage, and xorb closure;
5. resolves required xorb hashes through the pinned catalog;
6. fetches missing xorbs concurrently into an immutable local cache;
7. verifies xorb framing and every consumed chunk;
8. streams terms in recipe order while hashing the reconstructed file;
9. accepts output only when byte count and final file hash match.

Corruption, absence, authorization failure, or incomplete coverage returns an
error. Partial output is never reported as a hydrated file.

### 8.4 Mount, browse, and range reads

Mount and repository browsing reuse the same pinned resolver. File-range reads
map requested byte spans to shard terms and fetch only intersecting xorbs.
Prefetch coalesces requests by xorb identity across adjacent files. Xorbs remain
the cache and storage-tier unit, so no capsule range dependency is introduced.

### 8.5 Incremental pull

Pull first performs the standard Git fetch against one captured repository
view. Worktree updates then resolve new pointer versions through that same view
or explicitly open a later one after Git completes. One file reconstruction
never mixes shard or xorb catalog entries from two views.

## 9. Request and latency accounting

Let:

- `Xw` be newly written xorb objects;
- `Sw` be newly written shard objects;
- `V` be transport attempts needed to verify old external xorbs or shards
  outside the pinned base;
- `B` be checkpoint/frontier reads needed to materialize an uncached base
  file/xorb catalog;
- `P` be additional multipart operations beyond one single-object PUT;
- `R` be ref-registry transport attempts;
- `G` be exceptional GC-publication-guard transport attempts.

With an already captured remote-helper view, the pointer-free single-ref commit
path is one ref-head GET, one immutable leaf PUT, one conditional ref-head
PUT, and one root GET confirming that restore did not rotate ref authority:
four successful requests on a checksum-qualified store and five when
independent leaf readback is required. A cold explicit push uses one root GET
and direct GETs for the destination state instead of listing unrelated refs,
for six successful single-ref requests. Creating a ref also uses two
namespace-gate writes, for eight; these gates are partitioned by the
first component below `refs/<kind>/`. A full clone, fetch, or advertisement
instead adds two ref-head LISTs, one GET per visible ref head, one GET per
distinct prepared multi-ref activation, and the bounded run/checkpoint reads.
Those reads are parallelizable and do not serialize writers.

A clean multi-ref publication touching `N` refs adds one preparing-record PUT,
`N` conditional ref-head PUTs, one transaction-record CAS, and one immutable
committed-marker PUT to the common immutable work and the `N` ref-head reads.
The transaction record and marker are unique per attempt, so this adds requests
but no repository-wide mutable contention. Readers perform no global marker
scan and no GET per historical transaction: they resolve only activation
records still named by prepared heads.

With repository-local payloads and no bucket registry, a single-PUT pointer
push needs at least:

```text
qualified: 4 + B + Xw + Sw + P
readback:  5 + B + 2Xw + 2Sw + P
```

Canonical bucket-global xorbs and shards additionally require registry
protection, and cross-repository reuse outside the pinned base requires a GC
publication guard. Their complete request formulas are:

```text
qualified global: 4 + B + Xw + Sw + V + P + R + G
readback global:  5 + B + 2Xw + 2Sw + V + P + R + G
```

An uncontended registry GET plus CAS normally makes `R = 2`. In the current
implementation `G` applies to every pointer-bearing push and is provider- and
coordination-implementation-dependent; Git-only pushes have `G = 0`. A warm
catalog makes `B = 0`; a cold writer loads the checkpoint and bounded visible
ref frontiers after pinning the view. The ref-head CAS waits for immutable
writes, registry union, and the capsule. Narrowing GC admission requires a
separately proven reader/epoch protocol; it cannot be removed merely to improve
the request count.

An under-ten average is a valid gate for pointer-free pushes and measured
small-pointer workloads. It is not a valid universal bound for a multi-gigabyte
push containing many independent or multipart xorbs.

For hydration, let `H` be distinct uncached xorbs after coalescing every
requested file recipe. A cold operation requires approximately:

```text
1 root GET + 2 index LISTs + ref-head GETs + B catalog GETs
  + distinct shard GETs + H xorb GETs
```

The root GET disappears when checkout already supplies a pinned view. Immutable
cache hits remove corresponding catalog, shard, and xorb reads. Request count
therefore scales with reusable content containers, not chunks or file paths.

## 10. Concurrency and failure behavior

| Failure point | Reader-visible state | Recovery |
| --- | --- | --- |
| Before immutable upload | Old ref head | Return error |
| Partial xorb/shard upload | Old ref head | Abort multipart or retry by content identity |
| After payload upload | Old ref head plus unreachable objects | Reuse or collect after grace |
| After registry protection | Old ref head plus conservative retention | Registry compaction removes stale roots later |
| Ref-head CAS conflict | Same-ref winner only | Revalidate refs and dependencies; retry or reject |
| Ref-head response lost | Old or complete new ref state | Reconcile the exact canonical head body |
| Multi-ref heads prepared, record still preparing | All-old | Abort by record CAS or roll back exact prepared heads |
| Transaction record committed, marker absent | All-new | Recreate the exact immutable marker from the committed record |
| Committed-marker response lost | All-new | Read the exact record and marker; repair the marker if absent |
| Missing/corrupt dependency on read | No trusted reconstruction | Fail closed; repair from replica/source |

Ref locks are not required for updates to existing refs: the ref-head CAS is
the concurrency contract. Creation/deletion additionally coordinates only the
top-level Git directory/file namespace that can conflict. The GC publication guard is
required only for external xorb/shard safety; Git-only publication does not
touch that shared coordination object.

## 11. GC and registry lifecycle

### 11.1 Normal GC

Normal bucket GC:

1. pins registry coverage and a provider-backed age cutoff;
2. enumerates both capsule roots and legacy manifest roots during registry
   repair, rejecting a prefix that exposes both authorities;
3. authenticates each capsule root, checkpoint, and capsule frontier, then
   materializes its dependency-closed pointer catalog;
4. marks live shards and their complete xorb closures;
5. unions monotonic candidate shard roots not yet compacted;
6. excludes recent objects and incomplete multipart sessions;
7. revalidates candidate identity immediately before deletion.

The durable GC journal binds its plan to every capsule root digest as well as
the partitioned registry generations. A root change therefore invalidates a
paused or resumed plan before deletion. Registry repair replaces a v2 repo's
candidate roots with the complete authenticated catalog shard set; it must not
delete a repository merely because that repository has no legacy manifest.

A concurrent push is safe because its base-reachable dependencies are marked
from the old root and new dependencies are recent. An old dependency reused
from outside the base cannot race deletion because the writer holds the GC
publication guard before verifying it and until both registry and root
publication finish.

### 11.2 Registry compaction

Registry compaction is conservative maintenance. It may remove a candidate
shard root only after reading a stable repository-root generation and proving
that no retained root, checkpoint, capsule, protected-push session, or recovery
record references it. A conflict restarts that repository's compaction.

### 11.3 Shard layout compaction

`crab compact` selects v2 authority before considering the legacy shard-list.
It pins one authenticated repository view and derives its source shard set only
from the complete file catalog. The compactor hash-verifies source shards,
merges their file and Xorb records, removes unreferenced Xorb records, and
removes file records outside the authenticated catalog. It rejects an output
unless every authenticated file appears exactly once with its complete Xorb
dependency metadata.

Replacement shards are immutable. Before publication, the compactor uploads
them, verifies the complete replacement shard/Xorb/chunk closure from canonical
storage, and union-registers their roots. It then consolidates the pinned Git
packs and writes one checkpoint containing the full replacement pointer
catalog. The root compare-and-swap is against the exact captured root. Per-ref
updates made after capture remain as a visible suffix because their heads keep
the captured compacted transaction as predecessor. A root-CAS loser leaves only
safe immutable candidates and returns a conflict.

The checkpoint history segment retains the displaced checkpoint and capsule
frontier. Source shards are not deleted by compaction, and the monotonic
registry continues to retain their roots until separately proven registry and
history cleanup permits reclamation. A present corrupt v2 root fails closed;
only an absent v2 root selects the v1 shard-list path.

### 11.4 Xorb layout optimization

`crab optimize xorbs` selects v2 authority before the legacy manifest. Its
plan contains only Xorbs reachable from shards named by the authenticated file
catalog; it never treats another repository's objects in the shared global
namespace as optimization input. A corrupt present v2 root fails closed, while
an absent root retains the v1 inventory path.

Apply verifies each source Xorb, uploads immutable destination Xorbs, and
records the source-to-destination mapping in its resumable journal. On every
publication attempt it opens one current authenticated view, verifies that any
still-live source descriptor matches the source body, rewrites affected file
recipes, and strips file/Xorb records outside that view's catalog. Replacement
shards and their GC closures are uploaded and read-verified before publication.
The complete replacement catalog is then verified through every
shard/Xorb/chunk dependency, union-registered, and committed through the same
exact-view checkpoint CAS used by compaction.

A concurrent push either appears in the view being rewritten or wins the root
race. A losing optimizer retries from the newer root, so newly published files
that reuse a source Xorb are rewritten before the checkpoint becomes visible.
The v2 path creates no manifest, segmented shard list, SlateDB file index, or
generation receipt. Old Xorbs and shards remain immutable and conservatively
registered until later GC and registry cleanup prove them unreachable.

### 11.5 Forced GC

GC that bypasses age grace requires an exclusive maintenance generation. It
excludes GC publication guards and blocks ref publication and registry
compaction until deletion completes. It must never infer liveness only from a
stale checkpoint.

## 12. Recovery, replicas, and tiering

- Content hashes allow an xorb or shard to be repaired independently from a
  verified replica without changing the root.
- A location becomes readable only after complete hash verification.
- Storage-class transition preserves object identity and updates only derived
  location metadata when the provider changes versions.
- Restore state is operational metadata, not repository authority.
- Prepared mirror or recovery publication must verify the complete shard/xorb
  closure before invoking the same ref-head/transaction-record protocol.
- Active-active writers still require an external consensus authority; one
  regional object-store root is not cross-region consensus.

### 12.1 Native history and recovery without a foreground history write

V2 must not recreate the v1 global manifest as a history index. Doing so would
add a contended conditional write to every otherwise independent ref update.
The immutable leaf capsule already records the transaction identity, exact
base digest, ref edits, Git pack, and catalog delta needed for per-ref history.
It is therefore the foreground history record and requires no additional
object-store request.

Checkpoint maintenance must preserve that history before removing a compacted
capsule frontier. It writes one immutable, content-addressed history segment
containing the ordered transaction descriptors, predecessor segment hash,
affected refs, and the capsule hashes that retain Git and pointer dependencies.
The new repository root authenticates the segment tip, and the existing
checkpoint root CAS installs both together. Checkpoint and history writes run
in parallel. The segment is canonical and content-addressed, so an exact retry
reuses the same object. This adds one immutable PUT per checkpoint, not per
push, and does not introduce a repository-wide foreground mutex.

History operations follow the same authority rules as normal reads and writes:

1. `list` pins one root plus ref-head collection, then walks the authenticated
   current capsule suffix and history segments. Orphan capsules and uncommitted
   prepared transactions are excluded.
2. `verify` reconstructs the selected transaction's Git and pointer dependency
   closure and hash-verifies every pack, shard, and xorb before reporting it as
   recoverable.
3. Per-ref `restore` publishes a new ordinary v2 transaction from the current
   visible value to the selected historical value. It never rewinds a mutable
   head, overwrites an old root, or creates v1 metadata.
4. A whole-repository restore selects an authenticated checkpoint recovery
   point, first checkpoints the displaced current state, and rotates the ref
   authority epoch in the same CAS that acquires the maintenance fence. Old
   ref heads remain immutable evidence but are invisible; writers confirm the
   epoch after their CAS and fail retriably if restore won the race. One later
   root CAS installs the historical refs, HEAD, visibility, packs, and current
   append-only xorb/shard catalog as a new generation. Independent per-ref
   publications have no truthful global order, so a transaction on one ref
   must not be presented as an atomic snapshot of every other ref.
5. GC retains every segment, referenced capsule, Git pack, shard, and xorb in
   the configured recovery window. Pruning publishes a new authenticated
   segment frontier before any newly unreachable immutable object is eligible
   for normal grace-period collection.

The v1 generation-only CLI cannot identify concurrent per-ref history without
inventing an order. The v2 hard cutover therefore needs transaction/ref
selectors for per-ref recovery and checkpoint identifiers for full-repository
recovery. Migration must translate retained v1 manifest roots into checkpoint
recovery points before v1 authority is removed.

## 13. Security and authorization

Git visibility and file visibility are exact-view-bound. Authorization to read
a pointer does not imply authorization to enumerate arbitrary shard or xorb
keys. Product endpoints resolve authorized file requests through the pinned
catalog and issue only the required storage operations.

Direct object-store deployments necessarily rely on scoped credentials. Their
policy must restrict repository metadata and the required global immutable
prefixes without granting mutation of another repository's root or registry
record.

## 14. Observability

Record, without object keys or repository secrets:

- pointer files and distinct file versions;
- chunks classified as base-reachable, new, or externally reused;
- xorbs and shards created, reused, conflicted, and verified;
- bytes uploaded, skipped by deduplication, downloaded, and reconstructed;
- immutable requests, registry requests, root requests, retries, and
  sequential latency waves;
- cache hit ratio and hydration xorb fan-out;
- orphan objects and conservative registry entries collected;
- reconstruction and integrity failures by stage.

## 15. Initialization and v1 cutover

### 15.1 New repository

Initialization:

1. validates that the repository prefix is empty or already contains the exact
   same v2 identity;
2. creates the versioned ref-registry discovery record;
3. creates generation-zero `v2/root` with pointer capability disabled;
4. enables pointer capability only after a valid empty pointer checkpoint and
   every required reader contract are installed.

An initialization failure may leave an unused registry record, but never an
advertised partially initialized repository.

### 15.2 Existing v1 repository

Cutover reuses verified canonical xorbs and shards without copying them:

1. stop all v1 writers for the repository scope;
2. pin and verify the authoritative v1 manifest, refs, packs, shard set, xorb
   closure, visibility state, and registry coverage;
3. build a v2 checkpoint containing the equivalent Git, file, shard, xorb, and
   visibility catalogs;
4. verify every external object referenced by that checkpoint;
5. publish the versioned registry discovery record and complete candidate
   shard closure while GC publication is excluded;
6. create the initial v2 root pointing to the verified checkpoint;
7. fresh-clone, hydrate representative and boundary-size files, run full Git
   fsck, and compare complete file digests;
8. enable v2 writers and permanently reject v1 publication;
9. remove obsolete v1 repository metadata only after retention and exact-scope
   GC prove it unnecessary.

The cutover has no dual writer and no reader fallback. A v2 root must never
reference a v1 database row whose storage engine state is not represented by
the v2 checkpoint.

## 16. Implementation status

Implemented:

1. `CatalogDelta` carries versioned external file, shard, new-xorb, and ordered
   chunk-placement descriptors. Dependencies authenticated by the pinned base
   are named by each new shard closure without repeating their descriptors;
   delta application validates the complete combined closure. `FileData` and
   `FileRecipes` remain unused.
2. Checkpoints compact the complete pointer catalog, and read views apply
   checkpoint plus capsule deltas against one authenticated root.
3. Single-ref publication uses only that ref's conditional head update;
   multi-ref publication uses prepared two-version heads, one unique
   commit-vs-abort transaction record, and one immutable committed marker.
   Readers double-collect ref-head object versions and resolve each referenced
   activation record once. The repository root is a checkpoint and maintenance
   authority, not a foreground push mutex.
4. Pointer push consumes caller-verified canonical staging recipes without a
   redundant whole-file reconstruction, reuses base-generation chunk
   placements, fully reads and verifies cross-repository xorb candidates while
   holding GC publication admission, fully verifies adopted add-time xorbs,
   hash-checks newly read chunks, builds bounded canonical xorbs for remaining
   chunks, adopts only fully authenticated existing encodings after logical
   xorb create conflicts, and finalizes dependency-closed shards.
5. Xorbs use bounded parallel verified create-only writes, and shards use
   verified create-only writes, before their closure is unioned into the
   bucket registry and before capsule/ref publication. Successful writes
   warm verified local and optional service caches; cross-client cache-service
   hits are only candidates and still require a full canonical-origin proof.
6. Pointer-bearing pushes hold global and repository GC writer admission across
   external-object verification, registry union, and ref publication; Git-only pushes
   retain the capsule-protocol path without those leases.
7. The shared file-index session selects the complete v2 checkpoint plus
   visible per-ref capsule catalog when a v2 root exists,
   so clone checkout, smudge, hydrate, prefetch, diff, and mount retain the one
   canonical shard/xorb reconstruction path.
8. Repack preserves the complete catalog without rewriting xorb or shard
   payloads, records exact compacted positions for every ref, and readers
   discard the whole compacted history prefix rather than only its last
   transaction.
9. Foreground ref publication appends one immutable leaf capsule and never
   performs history-dependent carry reads. Server maintenance checkpoints at
   32 visible capsules. A foreground checkpoint is forced at 56 capsules if
   maintenance falls behind; per-ref frontiers reject more than 64 entries if
   maintenance still cannot preserve the bounded-read contract.
10. Readers retain authenticated predecessor edges from every per-ref
    frontier while ordering capsules. Expected-old OIDs remain a consistency
    check, but do not define causality by themselves: a force-push sequence
    such as `A -> B -> A -> C` must not make the final `A -> C` capsule eligible
    before the intervening transactions.

### 16.1 V1 product-parity inventory

Protocol v2 is not release-equivalent to v1 merely because ordinary push,
clone, fetch, pull, Xet hydration, repack, fsck, and GC work. Parity requires
every shipped user operation to either use v2 authority or be intentionally
removed as a product decision. No command may silently fall back to v1, and an
explicit `not yet part of the capsule protocol` error is a parity blocker.

| Surface | Current v2 state | Work required for parity | Acceptance proof |
| --- | --- | --- | --- |
| Repository initialization and ordinary single-/multi-ref push | Implemented with per-ref heads and transaction records | Qualify provider conditional-write and uncertain-response behavior | Concurrent same-ref and disjoint-ref pushes on S3, GCS, and Azure; fresh clone and fsck after every run |
| Full clone, fetch, pull, and ref advertisement | Implemented for complete repository views | Bound full-view read amplification as ref count grows; add derived indexes only if measurements require them | Repositories with thousands of refs; exact refs, byte-identical checkout, strict fsck, bounded requests and memory |
| Shallow, deepen, unshallow, filtered/partial, and raw-object/promisor fetch | Filtered transfer uses terminal Git protocol-v2. The classic helper advertises shallow support, pins one authenticated capsule view, uses the same canonical upload-pack planner for shallow/deepen/unshallow, generates a self-contained pack, serializes local installation, and transactionally updates `.git/shallow`; relative deepening and follow-tags are covered by an end-to-end helper test. Raw-OID recovery uses the same pinned view and authorization proof, then atomically installs the selected pack plus `.promisor` sidecar. RustFS qualification covers the initial filter matrix and lazy retrieval | Complete released-shape, older-Git, hosted-provider, interrupted-resume, hidden-ref, cancellation, and adversarial transport qualification | Git compatibility matrix for every fetch mode, including lazy recovery after process restart, interrupted installation, hidden-only objects, and adversarial missing objects |
| Explicit tag push | Uses the ordinary ref transaction; `crab push --follow-tags` adds only missing reachable annotated tags, and `--no-incremental` publishes the full outgoing Git/LFS closure | Complete hosted-provider and adversarial multi-ref qualification | Annotated/lightweight tag creation, replacement, deletion, atomic branch-plus-tag push, follow-tags missing-only behavior, and full-closure clone/fsck |
| Managed/protected push and active-active publication | Direct and protected active-active pushes bind the exact v2 base root, transaction, activation, capsule run, ref edits, and verified dependency closure in coordinator truth, materialize per-ref heads after consensus, preserve coordinator metadata in the client result, and retain ordered regional repair records. Active-active mirror plans replicate their immutable intent and repair terminal receipts after a replacement regional activation. Protected admission selects v2 authority before any v1 compatibility read, double-reads only the destination ref heads, resolves transaction-consistent per-ref state without repository-wide LIST or capsule payload downloads, fails closed on corrupt v2 metadata, and persists the exact root digest plus authorized old OIDs. The client stages the thin capsule and its Xet/LFS dependencies under the authorization grant without mutating GC or ref state. Direct-source verification binds the staged run, Git closure and visibility, changed paths, Crab shard/xorb closure, LFS bodies, and complete staged-object inventory. Finalize revalidates its evidence, promotes immutable dependencies, registers verified shard roots, and recognizes the exact already-visible transaction on retry. Path-scoped v2 views publish native capsules with authenticated Git visibility, external xorb/shard catalog entries, LFS dependencies, GC roots, and a fail-closed readiness record. Protected filtered pushes deterministically synthesize source commits, preserve hidden paths, carry required view-local shard/xorb bodies into source storage, and retry against the same source transaction. The integration path proves pointer identity, byte-identical Xet reconstruction through the published source catalog, and LFS body equality | Complete RustFS, Crab Auth, and managed-provider active-active qualification | Deny/allow/stale-policy races, pointer and LFS view pushes, lost responses, regional failover, ordered repair, receipt recovery, and all-old/all-new multi-ref visibility |
| Xet add, dedup, push, clone checkout, smudge, hydrate, prefetch, and diff | Whole-object RustFS path implemented | Finish hosted checksum/multipart, cross-repository reuse, cache, and corrupt-object qualification | Byte equality, dedup accounting, retry safety, and integrity failures across supported providers and object sizes |
| FUSE/NFS mount | Shared v2 file-index and hydrator wiring implemented | Qualify range reads, cold/warm cache, eviction, cancellation, unmount, and restored-tier objects | Mount/read/stat/range/concurrent-reader suite on every supported mount platform and provider |
| `download`, `export`, and remote `run` inputs | Remote snapshot materialization now resolves refs and installs Git packs from one authenticated v2 view; direct RustFS file equality is proven | Complete every revision form, selector shape, pointer payload, missing/corrupt-pack, and cancellation case | Output equality against a local clone for `download`, `export`, and workflow `--pull` |
| Import publication | Canonical staging recipes now publish through the one v2 capsule publisher; imports commit portable Crab configuration, account newly created xorb bytes, preserve empty files, and create no v1 manifest or file-index metadata | Complete hosted-provider, interrupted-resume, cancellation, and cross-import dedup qualification | Large-file import, resume, cancellation, dedup, clone, hydrate, and fsck without a v1 manifest |
| HTTP server, repository browser, smart Git receive, and server maintenance | V2-only catalog initialization, browser reads, smart receive, protected/app publication, replay receipts, HEAD updates, and background checkpoints are implemented; background, foreground, and manual checkpoints share one verified complete-pack consolidation path | Qualify checkpoint byte growth and complete hosted-provider/load qualification | Browser and smart-HTTP read/write/auth/fault/maintenance suites plus long-run clone/fetch and request/byte measurements against a v2-only repository |
| S3 gateway read and mutation | Repository reads select a present v2 root exclusively, open authenticated capsule packs and refs through the shared remote Git reader, cache by the exact v2 state digest, and fail closed instead of falling back to v1. Gateway mutations embed their generated Git pack and visibility proof in one capsule, publish through the per-ref head, recover multipart retries from v2 receipts, share the verified checkpoint path, and synchronously checkpoint a busy ref at 56 capsules before its hard bound; v1 repositories retain their journal owner | Complete the full S3 operation, concurrency, restart, request-count, and real-provider qualification matrix, then decide the separately scoped v1 retirement policy | S3 read/list/write/delete/multipart semantics, corrupt-v2 rejection, concurrent mutations, sustained-write checkpoint, restart, request-count, and clone/fsck verification |
| Repack, repository GC, bucket GC, and fsck | V2 checkpoint publication writes one authenticated history segment in parallel with the checkpoint; repository GC walks the bounded segment chain and retains every referenced checkpoint and capsule run; fsck authenticates the chain and all immutable dependencies before reporting the repository clean. History pruning rebuilds the retained immutable chain under the repository GC fence, atomically swaps only the authenticated root frontier, and leaves physical deletion to grace-period GC. Current pointer catalogs are append-only for shard/xorb identities, so bucket GC retains historical external-data dependencies through the current authenticated catalog | Complete crash/fault, multi-segment prune, and forced-GC concurrency qualification | Injection at each publication and prune boundary; resurrection, restart, no reachable deletion, and bounded writer pause |
| Replica selection, readiness, repair, and active-active reconciliation | Read selection requires an exact authenticated v2 state digest and verified shard/xorb bodies. Capsule-backed coordinator gaps replay by monotonic commit sequence, verify the exact run/ref transaction plus the resulting pointer catalog before per-ref visibility, and remain idempotent; v1 transactions retain manifest repair | Complete managed-provider failover/failback and fault qualification | Lag, partial replication, corrupt replica, failover/failback, ordered/idempotent repair, and concurrent publication matrix |
| Tiering and archive restore | Canonical xorb identity is reusable, but v2 reachability integration is unqualified | Drive lifecycle and restore decisions from v2 reachability while keeping restore state non-authoritative | Transition/restore/hydrate/mount/GC race tests for every supported storage class |
| Doctor, history inspection/restore, and v1-to-v2 cutover | Remote doctor selects and verifies a present v2 root before considering the validated v1 layout, identifies the v2 generation, accepts v2-only repositories, and fails closed on corrupt v2 authority. Checkpoint maintenance publishes a deterministic authenticated history-segment chain without adding a foreground push request. `recover history` selects that v2 authority for list, strict dependency/Git verification, restore preview, fenced retention apply, and atomic restore-as-new publication. Restore checkpoints the displaced state, rotates ref authority at fence acquisition, preserves the append-only xorb/shard catalog, installs refs and HEAD in one root CAS, and never reads a v1 manifest after v2 selection | Add transaction/ref selectors beyond checkpoint recovery points, complete doctor reporting, the offline verified one-way migration command, and live crash/concurrency qualification of restore | Migrate a populated v1 repository, reject dual authority, list and verify retained checkpoints, prune without reachable deletion, race restore with writers, restore as a new generation, then fresh-clone/hydrate/fsck |
| Mirror plans and reconciliation | V2 intent/terminal receipts, marker repair, hook delivery, interruption, cache exclusion, deletion approval, and metadata-staleness behavior are qualified on RustFS | Complete authorization and hosted-provider behavior | Repeated crash-resume and duplicate-delivery runs with exact final refs and no partial transaction |
| Git LFS and backup/restore | Canonical v2 push publishes and verifies reachable LFS dependencies before ref visibility. Direct LFS pre-push selects v2 authority first and reads transaction-consistent remote tips from the root and ref heads without downloading capsule payloads; a corrupt v2 root fails closed, while v1 fallback occurs only when the v2 root is absent. Mirror-hook push plus fresh hydrated clone are qualified on RustFS | Qualify direct LFS endpoint modes and make repository-prefix backup/restore discover all v2 authority and dependencies | LFS push/clone plus backup/delete/restore/fresh-clone/fsck/hydrate on a v2-only repository |
| Repository lifecycle, locks, releases, workflows, ship, and app mutations | Several paths publish through the canonical v2 server transaction, but the complete shipped command/route set is not yet audited | Bind every mutation to a v2 view and transaction; remove or explicitly retire every manifest/journal path | Create/update/delete, archive/freeze, lock races, release lifecycle, workflow restart, and ship E2E against a v2-only repository |
| Diagnostics, accounting, and administration | V2 fsck/GC and checkpoint-history inspection/pruning have canonical paths, and the ordinary doctor remote check diagnoses v2 authority without requiring v1 metadata. `crab metadb diagnose` now selects a present v2 root exclusively, uses a payload-free root/ref probe by default, and authenticates the complete stable checkpoint/capsule/catalog/visibility/Git-pack view plus every shard and xorb under `--deep`. `crab metadb rebuild` verifies the same complete external closure and publishes one exact-view checkpoint without creating legacy metadata. `crab metadb owner` fingerprints the transaction-consistent root/ref view and publishes exact-root-CAS checkpoints from that same pinned view. Recovery file-index verification likewise selects v2 first and checks the authenticated pointer catalog without acquiring a legacy writer or creating SlateDB state. `crab compact` and `crab optimize xorbs` now select v2 authority, derive inputs only from the authenticated catalog, verify their complete replacement shard/Xorb closure, and publish replacement catalogs as exact-view checkpoints without creating v1 metadata. Cost inventory counts the complete configured repository prefix, including v2 roots, ref heads, transactions, capsules, checkpoints, releases, and workflow objects, alongside shared xorb/shard storage without attributing sibling repositories. Plain hydration `status`, local logs/audit/stat, and local cache/staging usage are format-neutral; `du --remote` already walks the configured repository prefix plus shared content. Combined optimize orchestration, workflow/DAG inspection, and related remote administration still have mixed or unproven authority | Define every remaining remote answer from the pinned v2 root/ref/checkpoint/capsule closure or retire the command; never synthesize a v1 manifest. Reduce continuous-owner ref-head polling amplification with root-authenticated aggregate activity evidence without reintroducing a contended mutable root. Add command-level proof for the format-neutral surfaces instead of treating them as protocol adapters | Command-by-command golden outputs, corruption injection, cancellation, bounded-memory scans, and proof that no v1 metadata is read or recreated |
| Local cache, worktree, hydrate/dehydrate, and pointer tooling | Core reconstruction uses the shared v2 file index. Post-clone/fetch shard warming selects v2 authority first, derives the complete shard set from the authenticated pointer catalog without a duplicate root read, and fails closed on corrupt v2 metadata; v1 fallback occurs only when the v2 root is absent. Many remaining operations are local and format-neutral | Audit remote refresh, cache invalidation, prune, multi-worktree, and recovery edges against v2 view identity. Measure catalog-read byte amplification: the current authenticated reader verifies complete checkpoint and capsule objects, so introduce a root-authenticated catalog index only if qualification shows that embedded Git-pack bytes materially hurt fetch latency | Cold/warm/missing/corrupt cache, multiple worktrees, interrupted hydrate/dehydrate, prune, pointer conversion matrix, and catalog request/byte counts over long histories |

This table is a capability inventory, not permission to leave unlisted entry
points behind. Before release, a generated or reviewed ledger MUST map every
shipped CLI subcommand, remote-helper verb, HTTP route, S3-gateway operation,
background worker, and administrative task to exactly one row and one of:
`v2 proven`, `intentionally removed`, or `release blocker`. Adding a new entry
point without a ledger owner fails the parity gate.

### 16.2 Parity closure order

Parity closes in dependency order:

1. **Complete Git semantics:** keep terminal advanced fetch on the canonical v2
   view, then close tag-option, managed/protected authorization, released-shape,
   older-Git, and active-active consensus gates.
2. **Remove v1 product adapters:** remote snapshot commands, import, HTTP
   server/browser, S3 gateway, lifecycle commands, workflows, releases, and
   diagnostics must use the same v2 read and publication contracts. No second
   publisher is permitted.
3. **Complete operations:** replica repair, tiering, doctor, history
   recovery, migration, LFS, backup/restore, and mirror restart behavior must
   understand v2 authority and reachability.
4. **Qualify every boundary:** hosted checksums and multipart transport, fault
   injection, concurrent normal and forced GC, mounts, caches, storage classes,
   replicas, and production-scale workloads must pass on every supported
   provider.
5. **Close the inventory:** map every shipped command, route, worker, and
   maintenance task to a passing parity row or an explicit product removal.

Release requires every row above to have Level 3 end-to-end proof or stronger.
Correctness rows involving publication, authorization, recovery, replication,
or GC additionally require adversarial failure proof. Performance acceptance
requires simple incremental pushes to remain under ten object-store requests
on average with latency flat over history, and full-view operations to remain
bounded and measured as refs and immutable history grow. A passing protocol
core does not waive a missing product adapter or qualification row.

### 16.3 Cross-surface parity contracts

The remaining replica, tiering, mount, and browser work is one integrated read
contract, not four independent checklists:

1. **Replica readiness:** a replica is selectable only after the exact root,
   captured ref heads, activation records, checkpoint, capsule suffixes, Git
   packs, shards, and xorbs for that view are present and hash-verified. Copying
   the mutable root first never makes a replica ready.
2. **Repair and failover:** repair copies immutable content from a verified
   source, verifies it at the destination, and only then advances derived
   readiness. Failover/failback cannot invent a second ref authority or make a
   partially replicated transaction visible.
3. **Tiering:** archive/restore state remains operational metadata. A restored
   xorb or shard becomes readable only after content verification; lifecycle
   transitions cannot change canonical identity or v2 reachability, and GC
   cannot delete the last readable copy while restore is pending.
4. **Mount:** lookup, stat, readdir, full read, and range read pin one v2 view.
   Concurrent publication may affect the next lookup but cannot mix recipes,
   shards, xorbs, or Git trees within an open read. Cancellation, cache eviction,
   unmount, failover, and archive restore must release resources without
   returning partial bytes as success.
5. **Browser and HTTP:** tree, blob, history, blame, archive/download, and
   large-file rendering use the same authorized pinned view as clone. Browser
   metadata never proves xorb availability; content endpoints resolve the
   catalog closure and verify reconstructed bytes before success.

Qualification MUST cover supported provider × primary/replica × hot/restored
storage × cold/warm cache boundaries. It need not run every Cartesian product,
but every pairwise boundary and these high-risk combined cases are mandatory:
replica failover during mount range reads, restore racing hydrate and GC,
browser download during checkpoint publication, corrupt primary repaired from
a lagging replica, and concurrent disjoint-ref pushes while clone and browser
sessions remain pinned. Each run finishes with an independent clone, strict Git
fsck, and byte-digest comparison for every exercised large file.

## 17. Verification gates

The feature is not complete until tests prove:

- one pointer push, fresh clone, hydrate, and byte-digest equality;
- incremental pointer replacement followed by fetch, pull, and hydrate;
- multiple files and repositories reuse the same xorb identity;
- interrupted upload and retry never publish a missing dependency;
- ten same-ref agents integrate without corruption, while 50 and 100 agents
  updating pre-existing distinct refs share no mutable publication object;
- branch creation/deletion separately proves Git directory/file namespace
  safety;
- a force push resurrecting old content is protected from concurrent GC;
- normal and forced GC retain every reachable shard and xorb;
- shard gaps, reordered terms, wrong sizes, corrupt chunks, corrupt xorbs, and
  corrupt catalogs fail closed;
- cold and warm hydrate, mount range reads, and cache eviction reconstruct
  identical bytes;
- tiny, large, and multipart pointer pushes pass on RustFS and every supported
  hosted provider;
- transport counters match the formulas in section 9;
- pointer-free push and clone performance do not regress;
- production qualification includes a large real repository plus synthetic
  large-file history, periodic fetch/hydrate, final independent clone, full
  Git fsck, and file-digest comparison.

### 17.1 RustFS large-file evidence

The `capsule-xet-qualified-20260915` run used the installed Crab 1.2.4 release
binary against a fresh local RustFS bucket with `run_add_push_scale_rustfs.py`.
It passed 265 checks with ten distinct non-zero 512 MiB files, 100 small source
files, one seed publication, and ten independently edited versions:

- the 55 GiB logical large-file history retained 5,490,783,581 xorb bytes
  (9.30%), growing from 90 seed xorbs to 190 xorbs and from one to eleven
  shards;
- incremental pushes averaged 3.423 seconds, with a 2.526-second median and
  7.567-second nearest-rank p95; no latency growth with history was observed;
- each ten-file incremental push averaged 48.5 object-store requests (range
  47–52): 21.5 GET, 4 HEAD, and 23 PUT attempts. RustFS used mandatory
  readback; the mean request and response bodies were 20.0 MB and 31.4 MB;
- a cold independent repository reused a 513 MiB source version while adding
  only two xorbs and 68,537,033 encoded bytes. Appending changes the prior EOF
  chunk boundary, so the terminal xorb and tail are legitimately new;
- independent consumer and primary clones hydrated byte-identically. The
  primary clone passed cold and warm hydrate/dehydrate cycles for every large
  and small file, pointer-shape checks, and strict full Git fsck;
- the v2-aware store checker subsequently re-read and verified the complete
  190-xorb, eleven-shard catalog from the independent clone in 51.5 seconds,
  with zero errors or informational findings.

The separate `fsck-v2-qualified-20260915d` destructive-GC run used a fresh
RustFS bucket and 10,000-object fixture. It passed live-object retention,
unreachable-object deletion, post-GC v2 fsck, byte-identical fresh-clone
readback, writer-race fencing, both injected crash-resume points, bounded
memory, and bounded writer pause. Peak RSS was 144,310,272 bytes and measured
writer pause was 349 ms.

The post-read-path `capsule-xet-current-20260915b` regression run passed 74
checks with two distinct 512 MiB files over five versions. Its 5 GiB logical
history retained 1,084,259,598 xorb bytes (20.20%); a cold independent
repository reused a 513 MiB file while creating only two xorbs totaling
68,537,033 bytes. Independent clone, two hydrate/dehydrate cycles, strict Git
fsck, and byte-digest comparisons all passed. The companion cache-service
RustFS run passed 1,258 checks: an independent client resolved all 18 queried
chunks, performed one canonical xorb GET and one shard GET, and performed zero
xorb PUTs; an injected cache-warm failure did not affect publication or later
byte-identical hydration.

The `v2-parity-partial-20260915-c` terminal Git run used the installed release
binary against a fresh repository prefix in the existing RustFS qualification
bucket. It passed all 92 assertions across 323 commands. Expected non-zero
commands covered stale-lease rejection, offline promised-object failure,
hidden/dangling/unknown OID rejection, and injected disconnects. The successful
matrix covered full and legacy clone, shallow/deepen/unshallow, filtered and
lazy fetch, raw-OID/promisor admission, ref lifecycle, ordinary
pull-rebase-push, security, and disconnect recovery. The formerly corrupting
multi-ref create/update, force-update-plus-delete, then single-ref successor
sequence completed with a readable peer pull. Separate v2-only command checks
downloaded and exported the same file and restored the same missing workflow
dependency through `run --pull`, with byte-identical outputs.

The superseding mirror-enabled `v2-parity-mirror-20260915-g` run passed 144
assertions across 512 commands from a fresh local cache and repository prefix.
Its 479 successful commands covered the terminal Git matrix plus composed
mirror hooks, real Xet and LFS dependency publication, pointer verification,
hydrated clone and strict fsck, initial plan/apply and historical replay,
metadata-only v2 checkpoint staleness, invalid-root fail-closed behavior,
cache ownership/exclusion, interruption recovery, deletion approval, and
provider-failure reporting. All 33 non-zero commands were expected rejection
or injected-failure cases. The qualification runner now faults the v2
authenticated root rather than the removed v1 layout/manifest and proves that
mirror verification neither needs nor recreates the v1 file-index database.

The isolated `v2-flags-rustfs-20260915` run used the installed release binary
against a fresh RustFS server and repository prefix. An initial
`--follow-tags` push atomically published the branch and its reachable
annotated tag. A successor `--follow-tags --no-incremental` push published the
full outgoing closure, advanced only the branch, and preserved the existing
remote tag after a conflicting local rewrite. A fresh v2 clone matched both
remote OIDs and passed `git fsck --strict`.

The isolated `v2-import-rustfs-20260915` run used the installed release binary
and a fresh RustFS bucket. A flat same-bucket import published 101,844,789
source bytes in 1.835 seconds and reported 59,475,287 newly created xorb bytes.
The first commit carried the canonical `crab://` locator, S3 provider hint,
extension globs, an exact extensionless-path attribute, and a zero-byte file.
A fresh eager clone completed in 1.149 seconds; all three files matched the raw
source byte-for-byte and strict Git fsck passed. A subsequent full dehydrate
and hydrate cycle restored all 101,844,789 bytes in 1.238 seconds. Store
inspection found the v2 root, ref head, capsule, xorbs, and shard, with no v1
manifest, refs, metadata, or file-index objects.

This qualifies the earlier ordinary RustFS whole-object path. The current
leaf/checkpoint implementation still requires a fresh 5,000-push replay.
Hosted-provider, multipart, replica, tiering, mount-range, browser/HTTP
hosted-load, S3-gateway, import fault/resume/provider, migration/recovery,
managed publication, command-surface inventory, and backup/restore coverage
remain explicit release gates; this evidence does not waive them.

## 18. Acceptance boundary

Protocol-v2 pointer publication is enabled only through the dependency-closed
path above. The writer still fails before ref publication when staging, catalog,
external-object, shard, or registry proof is missing; it never publishes a Git
ref whose large-file closure exists only in staging or v1 metadata.

The implementation is accepted only when it preserves v1's xorb/shard byte
efficiency and reconstruction behavior while demonstrating that v2 removes
unrelated metadata, lock, journal, and manifest request amplification.
