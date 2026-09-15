# Protocol v2 Xorb and Shard Integration

## Document metadata

| Field | Value |
| --- | --- |
| Project | Crab |
| Scope | Pointer push, clone, fetch, hydrate, mount, recovery, and GC |
| Status | Core implementation and large-file RustFS qualification complete; hosted-provider and fault qualification pending |
| Priority | Correctness, large-file efficiency, then request latency and throughput |
| Companion | [Capsule Publication Protocol](capsule-publication-protocol.md), [Push Pipeline Deep Dive](push.md), [Canonical Object Storage Layout V1](../architecture/object-storage-layout.md) |

## 1. Decision

Protocol v2 retains xorbs and shards as independent, content-addressed object
store objects. It does not embed their payload bytes in publication capsules.

The design combines the strongest responsibilities of both protocols:

- v2 owns foreground transaction publication through one repository-root CAS;
- xorbs retain chunk aggregation, compression, immutable identity, independent
  caching, storage tiering, repair, and cross-file reuse;
- shards retain complete file reconstruction terms;
- capsules authenticate the ref transaction and the exact external dependency
  set, but remain bounded metadata and Git containers;
- checkpoints compact derived catalogs without rewriting live xorb payloads;
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
   as one root generation.
4. Preserve stable xorb and shard content identities across files and
   repositories.
5. Avoid foreground existence probes for dependencies already proven by the
   pinned base generation.
6. Keep a pointer-free small push on the existing three-request qualified or
   four-request readback path.
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

1. **Durable before visible.** Root CAS occurs only after every newly required
   xorb, shard, Git object, capsule, and GC protection record is durable.
2. **Complete recipe.** A committed shard covers every byte of each file
   version it declares, in order, with no gap or overlap.
3. **Authenticated closure.** A capsule commits to every shard introduced by
   the transaction and every xorb required by those shards that is not already
   proven by its pinned base.
4. **Authoritative omission.** A writer may omit a payload only when the pinned
   base root and its verified catalogs prove that exact content identity
   reachable, or after it independently verifies and protects the shared
   canonical object.
5. **Snapshot reads.** A read uses one root digest and never combines catalogs
   or visibility from different generations.
6. **Independent verification.** Readers verify capsule, shard, xorb, chunk,
   file, and Git identities at their respective boundaries.
7. **GC protection precedes publication.** Bucket-global objects are registered
   conservatively before the repository root can reference them. Reuse outside
   the pinned base also holds a GC publication guard across verification and
   root CAS.
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
├── capsules/{first-two-hex}/{capsule-blake3}
└── checkpoints/{first-two-hex}/{checkpoint-blake3}
```

Xorbs and shards preserve the canonical v1 keys. They are immutable and use
create-only writes. Capsules and checkpoints are repository-local. The root is
the only mutable reader-visible publication authority.

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

### 6.2 Root

The v2 root continues to contain refs, generation, parent digest, checkpoint,
bounded capsule frontier, capabilities, GC fence, and root digest. It does not
inline file, shard, or xorb catalogs.

Pointer capability is advertised only when the root's checkpoint and frontier
jointly provide complete file and xorb catalogs for that generation.

### 6.3 Checkpoint

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

Advertisement opens and verifies `v2/root`. Pointer preparation concurrently
loads the referenced checkpoint and bounded frontier, then constructs one
generation-pinned file, shard, and xorb catalog.

The writer validates expected-old refs, fast-forward policy, pointer
visibility, catalog completeness, and the root's advertised pointer
capability. It does not refresh the root merely because local preparation is
slow; final CAS detects staleness.

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
and root CAS. Git-only pushes do not acquire it.

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

The registry union is monotonic before root publication. A failed push may
over-retain its candidate closure until registry compaction, but GC cannot
delete a candidate that a concurrent root CAS is about to publish: new objects
are protected by age grace, base-reachable objects by the pinned old root, and
externally reused objects by the publication guard.

### 7.6 Publish

Only after all immutable writes, verification, and registry protection succeed
does the writer conditionally replace `v2/root` using the version retained from
advertisement.

The root CAS is the sole linearization point:

- success publishes all ref edits and pointer dependencies together;
- precondition failure publishes none of them;
- a same-ref loser revalidates and normally fails expected-old or
  fast-forward policy;
- a disjoint-ref loser may rebase its capsule metadata on the new root without
  re-uploading verified xorbs or shards;
- an uncertain response is reconciled using exact transaction identity.

### 7.7 Cleanup

Success retires local staging ownership only after the committed root is
observed. Failure retains staged content for retry. Remote immutable objects
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

Pull first performs the standard Git fetch against one pinned root. Worktree
updates then resolve new pointer versions through that same generation or open
a later explicit generation after Git completes. One file reconstruction never
mixes shard or xorb catalog entries from two roots.

## 9. Request and latency accounting

Let:

- `Xw` be newly written xorb objects;
- `Sw` be newly written shard objects;
- `V` be transport attempts needed to verify old external xorbs or shards
  outside the pinned base;
- `B` be checkpoint/frontier reads needed to materialize an uncached base
  file/xorb catalog;
- `C` be binary capsule-run carry reads;
- `P` be additional multipart operations beyond one single-object PUT;
- `R` be ref-registry transport attempts;
- `G` be exceptional GC-publication-guard transport attempts.

The pointer-free clean path remains `3 + C` requests on a checksum-qualified
provider and `4 + C` with capsule readback.

With repository-local payloads and no bucket registry, a single-PUT pointer
push needs at least:

```text
qualified: 3 + B + Xw + Sw + C + P
readback:  4 + B + 2Xw + 2Sw + C + P
```

Canonical bucket-global xorbs and shards additionally require registry
protection, and cross-repository reuse outside the pinned base requires a GC
publication guard. Their complete request formulas are:

```text
qualified global: 3 + B + Xw + Sw + V + C + P + R + G
readback global:  4 + B + 2Xw + 2Sw + V + C + P + R + G
```

An uncontended registry GET plus CAS normally makes `R = 2`. In the current
implementation `G` applies to every pointer-bearing push and is provider- and
coordination-implementation-dependent; Git-only pushes have `G = 0`. A warm
catalog makes `B = 0`; a cold writer loads the checkpoint and bounded frontier
after pinning the root. Root CAS waits for immutable writes, registry union,
and the capsule. Narrowing GC admission and adding bounded parallel immutable
uploads require separate proof and qualification; neither may weaken the
durable-before-visible contract.

An under-ten average is a valid gate for pointer-free pushes and measured
small-pointer workloads. It is not a valid universal bound for a multi-gigabyte
push containing many independent or multipart xorbs.

For hydration, let `H` be distinct uncached xorbs after coalescing every
requested file recipe. A cold operation requires approximately:

```text
1 root GET + B catalog GETs + distinct shard GETs + H xorb GETs
```

The root GET disappears when checkout already supplies a pinned view. Immutable
cache hits remove corresponding catalog, shard, and xorb reads. Request count
therefore scales with reusable content containers, not chunks or file paths.

## 10. Concurrency and failure behavior

| Failure point | Reader-visible state | Recovery |
| --- | --- | --- |
| Before immutable upload | Old root | Return error |
| Partial xorb/shard upload | Old root | Abort multipart or retry by content identity |
| After payload upload | Old root plus unreachable objects | Reuse or collect after grace |
| After registry protection | Old root plus conservative retention | Registry compaction removes stale roots later |
| Root CAS conflict | Winner's root only | Revalidate refs and dependencies; retry or reject |
| Root CAS response lost | Old or complete new root | Reconcile exact transaction identity |
| Missing/corrupt dependency on read | No trusted reconstruction | Fail closed; repair from replica/source |

Ref locks are not required for ordinary root publication correctness. The GC
publication guard is required only for reuse of an old external dependency
outside the pinned base. An optional admission policy may reduce large
speculative uploads under contention, but it must be separately measured and
must not become an implicit correctness dependency.

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

### 11.3 Forced GC

GC that bypasses age grace requires an exclusive maintenance generation. It
excludes GC publication guards and blocks root publication and registry
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
  closure before invoking the same root CAS protocol.
- Active-active writers still require an external consensus authority; one
  regional object-store root is not cross-region consensus.

## 13. Security and authorization

Git visibility and file visibility are generation-bound. Authorization to read
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
3. Pointer push consumes caller-verified canonical staging recipes without a
   redundant whole-file reconstruction, reuses base-generation chunk
   placements, fully reads and verifies cross-repository xorb candidates while
   holding GC publication admission, fully verifies adopted add-time xorbs,
   hash-checks newly read chunks, builds bounded canonical xorbs for remaining
   chunks, adopts only fully authenticated existing encodings after logical
   xorb create conflicts, and finalizes dependency-closed shards.
4. Xorbs use bounded parallel verified create-only writes, and shards use
   verified create-only writes, before their closure is unioned into the
   bucket registry and before capsule/root publication.
5. Pointer-bearing pushes hold global and repository GC writer admission across
   external-object verification, registry union, and root CAS; Git-only pushes
   retain the capsule-protocol path without those leases.
6. The shared file-index session selects the v2 catalog when a v2 root exists,
   so clone checkout, smudge, hydrate, prefetch, diff, and mount retain the one
   canonical shard/xorb reconstruction path.
7. Repack preserves the complete catalog without rewriting xorb or shard
   payloads.

Still required before release qualification:

1. Hosted-provider tuning and multipart transport evidence for bounded
   parallel uploads on very large pointer pushes.
2. Fault injection around every external upload, registry update, and root CAS.
3. Concurrent repository and bucket GC qualification, including forced
   resurrection of old content.
4. Cross-repository dedup, replica repair, tiering, mount, and browsing matrix
   coverage on every supported provider.
5. Hosted-provider production workloads from section 17.

## 17. Verification gates

The feature is not complete until tests prove:

- one pointer push, fresh clone, hydrate, and byte-digest equality;
- incremental pointer replacement followed by fetch, pull, and hydrate;
- multiple files and repositories reuse the same xorb identity;
- interrupted upload and retry never publish a missing dependency;
- same-ref and disjoint-ref CAS races preserve expected Git semantics;
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

This qualifies the ordinary RustFS whole-object path. Hosted-provider,
multipart, mount-range, replica, tiering, and browsing coverage remain release
gates; this evidence does not waive them.

## 18. Acceptance boundary

Protocol-v2 pointer publication is enabled only through the dependency-closed
path above. The writer still fails before root CAS when staging, catalog,
external-object, shard, or registry proof is missing; it never publishes a Git
ref whose large-file closure exists only in staging or v1 metadata.

The implementation is accepted only when it preserves v1's xorb/shard byte
efficiency and reconstruction behavior while demonstrating that v2 removes
unrelated metadata, lock, journal, and manifest request amplification.
