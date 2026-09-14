# Object-Store Request-Minimal Protocol

## Document metadata

| Field | Value |
| --- | --- |
| Project | Crab |
| Scope | Push, clone/read, recovery, and garbage collection |
| Status | Implementation in progress; not wired to user-facing commands |
| Priority | Correctness, then request latency, throughput, and transferred bytes |
| Replaces | The v1 multi-object publication layout after an explicit cutover |
| Companion | [Push Pipeline Deep Dive](push.md), [Canonical Object Storage Layout V1](../architecture/object-storage-layout.md) |

### Implementation status

The first implementation slice is present but intentionally unreachable from
the released push and read paths:

- `crab-metadata::request_minimal` owns bounded, versioned, checksum-bearing
  repository-root, capsule, ref-transaction, and checkpoint-pointer contracts;
- `crab-write::request_minimal` initializes and opens a repository root,
  uploads and independently verifies a capsule, and publishes through one root
  CAS;
- `crab-read::request_minimal` loads the root and its bounded capsule frontier
  concurrently, verifying every size, content, transaction, and base binding;
- equal-size capsule runs merge as a binary counter, so a 500-push checkpoint
  window has at most nine run objects and contains six after push 500;
- the executable clean-path test proves exactly four object-store operations,
  including advertisement: root GET, capsule-run PUT, run GET, and root PUT;
- the checksum-qualified AWS S3 test proves exactly three operations, while
  custom S3 endpoints and other providers retain mandatory readback;
- the executable one-capsule read test proves exactly two object-store
  operations: root GET and capsule-run GET;
- a 500-push executable model proves 1,994 qualified object-store operations,
  or 3.988 per push including advertisement and binary carry compaction;
- CAS-loser, expected-old mismatch, payload corruption, and lost-root-response
  tests fail closed or reconcile through exact transaction identity.

The current CLI remains on v1. Git pack/sidecar ingestion into capsules,
capsule-aware clone/fetch, checkpoint construction, v2 GC, provider checksum
qualification, migration, and live qualification remain required before the
hard cutover.

## 1. Decision summary

Crab should introduce a hard-cutover protocol that publishes one immutable,
self-contained **capsule** and then atomically points one mutable repository
**root** at it. The clean small-push budget is:

| Capability | Complete push | After Git advertisement |
| --- | ---: | ---: |
| Provider validates a qualified cryptographic upload checksum | **3 requests** | **2 requests** |
| Crab must independently stream the uploaded capsule back | **4 requests** | **3 requests** |

The three-request path is:

1. `GET {repo}/v2/root` for advertised refs and its CAS version.
2. Conditional `PUT {repo}/v2/capsules/{fanout}/{capsule-hash}`.
3. Conditional `PUT {repo}/v2/root` against the version from step 1.

The first request normally already belongs to Git advertisement. The push
therefore adds one data request and one publication request. Commit count does
not affect the request count; every commit included in one push shares the
same capsule and root transition.

This is the minimum production design for ordinary S3, GCS, and Azure object
semantics. A single mutable object could theoretically combine data and
publication, but it would require portable access to historical object
versions, rewrite or strand repository data, and turn every repository into
one unbounded hot object. This design rejects that optimization.

## 2. Motivation

Crab's v1 layout separates Git packs, `.idx`, `.rev`, kind evidence, pack
metadata, origin receipts, xorb bodies, shards, segmented indexes, SlateDB
state, ref-journal records, locks, admission slots, GC fences, and manifest
history. Each object has a valid local responsibility, but a high-latency
remote store charges at least one network round trip for every responsibility.

A current optimized-v1 single-writer RustFS measurement of one small
same-branch push recorded 69 transport attempts: 37 GETs, 2 LIST pages, 5
HEADs, and 25 PUTs.
There were no 5xx responses or SDK retries, so the remaining amplification is
structural rather than provider instability. V1 cannot coalesce those objects
without changing read, recovery, and GC contracts.

For remote stores, elapsed time is approximately:

```text
push latency = sequential request waves × remote RTT
             + transferred bytes / available bandwidth
             + local pack, hash, and compression work
             + retries and contention
```

Concurrency hides independent transfers, but it cannot hide a long chain of
dependent metadata requests. The new protocol minimizes both total requests
and sequential request waves.

## 3. Goals

The protocol MUST:

1. Keep every newly visible ref reconstructable from durable, verified bytes.
2. Give readers one coherent repository generation.
3. Reject lost updates and invalid same-ref races.
4. Remain safe if a client, process, machine, or request fails at any point.
5. Prevent GC from deleting data required by a committed or in-flight push.
6. Preserve byte-identical Git and file reconstruction or return an error.
7. Use three requests for an uncontended small push on a checksum-qualified
   provider, including advertisement.
8. Add no foreground `HEAD`, `LIST`, lease, heartbeat, admission, journal, or
   GC-fence requests on that path.
9. Bound cold-clone metadata amplification through immutable checkpoints.
10. Scale request count by capsules or multipart parts, not commits, files,
    chunks, refs, or metadata record count.
11. Continue serving standard Git packfile responses for clone, fetch, pull,
    shallow fetch, partial clone, and lazy object recovery.

## 4. Non-goals

This design does not attempt to:

- preserve the v1 physical layout or support dual v1/v2 reads and writes;
- guarantee that concurrent losing writers upload zero redundant bytes;
- retain cross-repository deduplication when proving it would require remote
  point lookups in the foreground push;
- make retries, multipart parts, or provider throttling disappear;
- replace the external consensus authority required by active-active mode;
- count IAM, credential discovery, DNS, TLS setup, or non-object-store service
  calls as object requests.

## 5. Request accounting

A logical object-store request is one attempted provider operation. Every
retry counts again. Multipart initiation, every part upload, completion,
abort, range GET, full GET, HEAD, LIST page, DELETE, and conditional write are
separate requests.

Budgets describe an uncontended attempt with no transient failure. They MUST
be enforced by transport-level counters, not inferred from application cache
hits. A provider SDK that internally emits multiple HTTP operations must
report those operations separately.

### 5.1 Foreground budgets

| Operation | Qualified checksum | Independent readback | Notes |
| --- | ---: | ---: | --- |
| No-op after advertisement | 0 | 0 | The advertised root already proves the result |
| No-op including advertisement | 1 | 1 | Root GET only |
| Ref-only update | 3 | 4 | A small capsule preserves transaction history and recovery evidence |
| New small capsule, no carry | 3 | 4 | Root GET, run PUT, optional run GET, root CAS |
| Binary carry across `C` occupied levels | `3 + C` | `4 + C` | Each carried level adds one run GET; only the final merged run is PUT |
| Existing verified capsule | 4 | 4 | Create conflict requires body verification before reuse |
| New multipart capsule with `P` parts | `P + 4` | `P + 5` | Root GET, initiate, parts, complete, optional GET, root CAS |
| Root CAS conflict | `+2` per retry | `+2` per retry | Refresh root, revalidate/merge, retry CAS |
| Uncertain root CAS response | `+1` | `+1` | Read root and classify the exact attempted transition |

The budget is per push, not per commit. A push containing one thousand commits
still uses one capsule upload if it fits the selected upload mechanism.

### 5.2 Latency waves

The checksum-qualified clean path has three ordered waves:

```text
client                         object store
  |---- GET root ------------------->|  advertisement and CAS base
  |<--- refs + version --------------|
  |---- PUT capsule, create-only --->|  durable verified bytes
  |<--- checksum/version ------------|
  |---- PUT root, if-match --------->|  sole publication point
  |<--- new version -----------------|
```

Local capsule construction may overlap advertisement. The data PUT cannot be
skipped, and the root CAS cannot start until capsule durability is proven.
Those dependencies define the minimum critical path on an object store with
no multi-object transaction.

## 6. Storage layout

Protocol v2 has one mutable foreground object and immutable capsules:

```text
{repo_prefix}/v2/
├── root
├── capsules/{first-two-hex}/{blake3}
├── checkpoints/{first-two-hex}/{blake3}
└── gc/runs/{run-id}/...
```

`root` is the only mutable publication authority. Capsules and checkpoints
are immutable and use create-only writes. GC state is maintenance-only and
MUST NOT be touched by a normal push.

There is no v2 foreground dependency on bucket-global xorbs, shards, indexes,
or a ref registry. Background deduplication MAY produce derived data, but the
root MUST remain readable when that derived data is absent.

### 6.1 Root

The root is a bounded, checksummed binary record containing at least:

```text
format_version
repository_id
generation
parent_generation_digest
refs[]                    // name, object ID, peeled ID when applicable
capsule_frontier[]        // hash, size, checksum, transaction identity
checkpoint                // hash, size, covered generation
checkpoint_pack           // capsule, byte range, Git checksum, object count
delta_depth
capabilities
root_digest
```

The complete advertised ref map is inline so advertisement requires one GET.
Implementations MUST define a maximum encoded root size. A repository that
cannot fit its refs under that bound requires a separately qualified sharded
ref protocol and does not claim the three-request budget.

The root's object-store ETag and version are CAS tokens, not content hashes.
The encoded `root_digest` detects body corruption independently of provider
version metadata.

The development codec uses an eight-byte `CRBROOT2` magic, a big-endian format
version and payload length, a deterministic JSON payload containing only
ordered maps and integer/string fields, and a trailing BLAKE3 digest over the
envelope and payload. Readers reject oversized, non-canonical, truncated,
extended, or digest-mismatched records before trusting any referenced object.

### 6.2 Capsule

Protocol v2 removes standalone object-store keys for packs and their sidecars;
it does not remove the Git pack format. Git clients consume packfiles, and the
remote helper's protocol-v2 upload-pack boundary must continue producing one
valid Git packfile response. A capsule is the storage container for those Git
bytes and their evidence.

A capsule contains every new authoritative artifact for one push:

- Git pack bytes;
- Git object offsets, CRCs, reverse indexes, kinds, and delta-base evidence;
- newly required file data and xorb-equivalent frames;
- file reconstruction recipes and chunk locations;
- ref transaction and fast-forward evidence;
- catalog and visibility deltas;
- base generation, base root digest, and base capsule frontier;
- a fixed-size footer locating every section;
- per-section and whole-capsule cryptographic hashes.

The pack, `.idx`, `.rev`, metadata, receipts, and catalog deltas are sections
of one object rather than separate object keys. Readers use exact ranges from
the footer and validate every returned section.

The development capsule codec concatenates non-empty sections, followed by a
bounded deterministic footer, footer length, footer BLAKE3, and `CRBCAPS2`
magic. The first section is always the canonical ref transaction. Its BLAKE3
must equal the footer transaction identity, and its base-root digest must equal
the footer base. Every section has a contiguous offset, length, kind, and
BLAKE3 entry; gaps, overlaps, duplicate transaction sections, and corrupt
ranges fail closed. Each Git pack descriptor binds exactly one pack section,
standard `.idx`, deterministic `.rev`, and checksummed object kind/delta
locator, plus the Git trailer checksum and object count. Git evidence cannot
appear outside a descriptor or be shared across descriptors.

A push capsule's pack section may use `REF_DELTA` bases reachable from its
declared base root. It is therefore not automatically a valid response for a
fresh client. `OFS_DELTA` bases remain inside the same pack section because
their identity is a pack-relative byte distance. The footer records enough
information to resolve every `REF_DELTA` base through the pinned repository
view and to reject a missing or unauthorized base.

A capsule MUST be self-contained relative to the root generation on which it
is based:

- dependencies reachable from the base root may be referenced;
- every dependency not reachable from the base root MUST be included in the
  new capsule;
- a force-push that resurrects old, currently unreachable content MUST upload
  that content again unless the current root proves it reachable.

This rule is what permits lock-free GC safety. A cache or probabilistic filter
may prove that data should be uploaded, but it MUST NOT be the sole proof that
data may be omitted.

### 6.3 Checkpoints

Capsules form an immutable generation DAG. The root carries a deterministic
frontier so disjoint CAS losers can merge their already-uploaded capsules
without rewriting either capsule. Reading an unbounded frontier would move
request amplification from push to clone, so a checkpoint periodically
materializes a complete repository view:

- one ordinary, non-thin, self-contained Git pack covering the checkpoint's
  complete Git object catalog;
- the pack checksum, object count, byte range, `.idx`, and `.rev` evidence;
- full Git object locator;
- complete file and chunk reconstruction indexes;
- current visibility state;
- the generation and root digest it covers.

The checkpoint locator maps each Git object ID to its checkpoint or retained
capsule, pack-section base, pack-relative offset, encoded length, CRC, kind,
and delta-base evidence. Physical reads add the pack-section base to the
pack-relative offset; the latter remains available for `OFS_DELTA` validation.

The checkpoint pack is a storage optimization, not an authorization bypass.
It may be streamed unchanged only when the requested authorized object closure
equals its complete catalog. Hidden refs, partial-clone filters, shallow
boundaries, or any smaller selection require Crab to generate a pack containing
only the authorized selected objects.

The root points to one checkpoint and a bounded binary frontier of later
capsule runs. Level `L` contains exactly `2^L` complete capsules. Appending a
leaf merges equal-level suffixes like a binary counter; only the final merged
run is uploaded. With a hard checkpoint interval of 500 pushes, at most nine
runs are addressable and generation 500 has six. The amortized number of carry
GETs is less than one per push, while a reader fetches one object per set bit
in the post-checkpoint transaction count.

Checkpoint construction is background maintenance and is not part of the
clean push budget. A checkpoint becomes visible through the same root CAS and
must preserve an equivalent ref state.

The maximum delta depth is a protocol constant chosen from live clone and
push measurements. Background construction should normally publish a new
checkpoint before the bound is reached. If maintenance falls behind, the next
push MUST wait for or synchronously construct a checkpoint before publishing a
root that would exceed the bound. That exceptional push has a higher request
and byte budget; the implementation must report it separately. The bound must
not become an unbounded configuration surface.

## 7. Push protocol

### 7.1 Prepare locally

Before remote mutation, the client:

1. parses and validates requested ref edits;
2. constructs all new Git and file data;
3. builds the capsule and its footer;
4. computes every section hash and the capsule BLAKE3 identity;
5. verifies the complete local capsule once.

Local failures produce no remote state.

### 7.2 Read the publication base

The remote helper GETs `v2/root` once and retains its body plus ETag/version
through advertisement and push. Before upload it verifies:

- root format, identity, bounds, and digest;
- every expected-old ref value;
- fast-forward policy;
- that every omitted dependency is reachable from this root.

The push MUST NOT issue another root GET merely because local preparation took
time. The final CAS detects a stale base.

### 7.3 Upload the capsule

The client performs a create-only PUT at the BLAKE3-derived key. It sends a
provider-qualified cryptographic checksum when supported.

A successful response is sufficient only when provider qualification proves
that the endpoint validates that checksum before acknowledging durability.
Crab MUST NOT treat an ETag as a content checksum. The current `object_store`
contract exposes atomic create/update and ETag/version tokens, but its common
put result does not expose one portable verified checksum. The v2 storage
adapter therefore needs an explicit verified-put capability; otherwise it
performs one full streamed readback.

If create reports that the key already exists, Crab streams and verifies the
existing capsule before referencing it. A hash-shaped key alone is not proof
that the stored bytes are correct.

### 7.4 Publish the root

After capsule durability is proven, the client constructs a root containing
the new refs and capsule identity. It conditionally updates `v2/root` using
both ETag and version from advertisement.

This CAS is the only linearization point:

- success makes every ref edit and all capsule metadata visible together;
- precondition failure makes none of this attempt visible;
- readers can observe the old root or the new root, never an intermediate
  combination.

No ref lock is required. A same-ref loser refreshes the root and fails the
normal expected-old or fast-forward check. Disjoint ref edits may be merged
onto the refreshed root and retry the CAS without re-uploading the capsule.

### 7.5 Reconcile uncertainty

If the root CAS response is lost or indeterminate, the client GETs the root
once and compares the attempted generation, parent digest, capsule identity,
and ref edits:

- an exact match is committed success;
- a descendant that contains the exact transaction is committed success;
- the unchanged base is safe to retry;
- any other state is an indeterminate error requiring explicit recovery.

Current refs alone are not sufficient proof because another writer could have
produced the same ref values.

## 8. Correctness argument

### 8.1 Durable-before-visible

The root cannot reference a capsule until its PUT and required verification
complete. A crash before root CAS leaves only unreachable immutable data. A
crash after a successful CAS leaves a fully verified reachable capsule.

### 8.2 Atomic ref updates

Every ref is encoded in one root. One conditional object update publishes a
multi-ref push atomically. There is no interval in which only part of a batch
is visible.

### 8.3 Lost-update prevention

Every mutation is conditional on the exact root version read during
advertisement or conflict recovery. At most one writer can replace a given
version. Losers re-evaluate semantic ref rules against the winner.

### 8.4 Snapshot reads

A reader validates one root and pins its digest for the operation. All
capsules and checkpoints referenced by that root are immutable. A later root
CAS cannot change the pinned view.

### 8.5 Reconstruction integrity

The root authenticates capsule identity and size. The capsule footer
authenticates section locations and hashes. Git objects retain Git object and
pack validation; file data retains file, chunk, and reconstruction hashes.
Any missing, short, oversized, reordered, or corrupt range fails closed.

### 8.6 GC safety without writer fences

Normal GC reads a root snapshot and traces every retained checkpoint and
capsule. It may delete an unreachable object only when all of these hold:

1. the object is older than the mandatory grace period;
2. it is unreachable from the GC root snapshot and retained history;
3. it is not part of a retained incomplete multipart session;
4. the deletion policy does not bypass concurrent-publication safety.

Age is evaluated against one provider-backed cutoff captured at the start of
the run. GC never advances that cutoff while scanning or deleting, so an
object created after the snapshot remains protected even when a long run
crosses the nominal grace duration.

A concurrent push can reference old data only when that data was reachable
from its base root; GC's snapshot therefore marks it. Data that was not
reachable from the base must be copied into the new, recent capsule, which is
protected by grace. A concurrent force-push may make old roots unreachable,
but that only causes conservative retention in the active GC run.

Forced GC that bypasses grace MUST acquire a maintenance generation through
the root CAS and block publication until it releases that generation. This is
an exceptional maintenance cost, not a normal push request.

## 9. Failure and concurrency behavior

| Failure point | Visible result | Recovery |
| --- | --- | --- |
| Before capsule PUT | No change | Return error |
| During single or multipart upload | No change | Abort if possible; lifecycle cleanup otherwise |
| After capsule PUT, before root CAS | Orphan capsule only | Reuse on retry or collect after grace |
| Root CAS conflict | No change from loser | GET root, revalidate, retry or reject |
| Root CAS succeeded, response lost | New root may be visible | One exact reconciliation GET |
| Reader sees corrupt root | No usable snapshot | Fail closed; recover from retained root/capsule evidence |
| Reader sees missing/corrupt capsule | Root is damaged | Fail closed; repair from replica or retained source |
| Client dies after success | Complete new generation | No lease expiry or cleanup required |

The protocol deliberately accepts speculative upload waste under contention.
Adding remote admission or locks would improve wasted-byte behavior by
increasing the clean request budget and latency. Clients instead use bounded
local concurrency, randomized CAS backoff, and reuse already-uploaded
capsules.

## 10. Clone and read protocol

Capsules are an object-store layout. The Git-facing protocol remains standard
upload-pack: advertisement and negotiation select objects, then Crab emits one
valid packfile stream. Multiple capsule bodies are never concatenated or sent
as multiple packs. Protocol v2 does not depend on Git `packfile-uris`.

### 10.1 Open and advertise

Every clone, fetch, pull, shallow fetch, partial clone, and lazy object request
first opens one immutable repository view:

1. GET and validate `v2/root` once;
2. pin its generation, digest, refs, checkpoint, and capsule frontier;
3. load checkpoint metadata and at most the bounded post-checkpoint metadata;
4. validate that the combined locator, catalog, and visibility proof cover the
   exact pinned generation;
5. advertise refs from the pinned root, applying hidden-ref policy.

No later root is mixed into the operation. A root CAS after step 1 creates a
new generation for another operation; it cannot change the pinned view.

### 10.2 Fresh full clone

A fresh clone has wants and no haves. Crab:

1. authorizes the requested advertised refs and computes their complete
   reachable Git object closure;
2. compares that closure with the checkpoint pack catalog;
3. if the root is exactly at the checkpoint and the authorized closure equals
   the complete catalog, range-GETs and streams the checkpoint pack section;
4. otherwise reads the checkpoint pack plus at most `D` post-checkpoint pack
   sections, where `D` is the hard delta-depth bound, and consolidates the
   selected objects into one self-contained non-thin response pack;
5. verifies the response pack's object catalog and trailer before writing the
   upload-pack `packfile` section;
6. lets Git validate, index, and install the pack normally.

The direct checkpoint path is forbidden when hidden refs, authorization,
partial-clone filters, or shallow boundaries make the requested closure
smaller than the checkpoint catalog. Those requests use selected-object pack
generation so unrequested or unauthorized objects do not cross the wire.

### 10.3 Incremental fetch and pull

For an incremental fetch, Crab validates wants and client haves against the
pinned visibility proof, computes objects reachable from wants but not from
common haves, and resolves the selected object IDs through the capsule-aware
locator. Adjacent encoded entries are combined into bounded range reads.

Every selected object is reconstructed and Git-object-ID verified. Delta bases
are recursively read from the pinned view unless the response is thin and the
base is a client-proven common have. Crab then writes one response pack:

- a self-contained pack when thin-pack negotiation is absent;
- a thin pack only when every omitted base is a proven common have.

`git pull` adds no remote storage protocol. It performs this fetch and then Git
merges or rebases locally.

### 10.4 Shallow, partial, and lazy fetch

Shallow fetch applies Git's requested history boundaries before pack
generation. Partial clone applies the negotiated object filter before reading
payloads. A later lazy fetch of a promised object repeats authorization for the
exact object ID, locates and verifies its capsule range and required delta
bases, and returns a small valid Git pack. None of these paths installs raw
capsule bytes into Git's object database.

### 10.5 Checkout and file hydration

Git pack transfer reconstructs the committed Git objects, including Crab
pointer objects. Checkout, hydrate, mount, and repository browsing resolve file
recipes through the pinned checkpoint plus frontier, coalesce the corresponding
capsule payload ranges, validate chunk and file hashes, and either reproduce
the exact file bytes or return an error. Native LFS traffic remains outside
these budgets until section 18's LFS protocol decision is closed.

### 10.6 Read request budgets

Let `D` be the number of post-checkpoint transactions, `popcount(D)` the
number of binary capsule runs, and `R` the number of coalesced ranges needed
for an incremental selection. Assuming the
root contains the checkpoint pack descriptor and one GET can return a complete
run or required contiguous pack range, the theoretical minima are:

| Operation | Minimum object-store reads | Qualification |
| --- | ---: | --- |
| Ref advertisement | **1** | Root GET |
| Full authorized clone at checkpoint generation | **2** | Root GET plus checkpoint pack range |
| Full clone ahead of checkpoint | **2 + popcount(D)** | Root, checkpoint pack, and each frontier run |
| Incremental fetch or pull | **1 + R** | Root plus selected coalesced ranges |
| Lazy object fetch | **2** | Root plus one range only when object and bases co-locate |

At the fixed 500-transaction checkpoint interval, `popcount(D) <= 8`, so an
unfiltered clone requires at most ten object reads and requires eight at the
500-transaction boundary. Over one complete 500-push window, binary carries
add `500 - popcount(500) = 494` GETs. The qualified single-PUT path therefore
uses `4N - popcount(N) = 1,994` total operations, or 3.988 per push; mandatory
readback uses 2,494, or 4.988 per push.

These are origin-request minima, not universal guarantees. A selected object
and its delta bases may span multiple runs; authorization or filtering may
force selected-object reconstruction; retries count again; hydrate and LFS add
their own reads. Claiming a constant two-request fetch would therefore be
incorrect.

A two-request clone for every generation would require publishing a complete
checkpoint pack with every push. That would replace request latency with
full-repository upload and repack cost and is rejected. The bounded `2 + D`
design amortizes checkpoint construction while enforcing a finite worst-case
source-capsule count.

Fresh-clone throughput should prefer full parallel capsule downloads when
consolidation is required. Partial clone, mount, and sparse hydration should
prefer coalesced ranges based on authenticated locators. The range planner
MUST merge adjacent sections only up to a bounded overfetch ratio so request
savings do not create uncontrolled byte waste.

Local caches are keyed by immutable capsule hash and section range. They may
remove repeated remote reads but never replace root, authorization, section,
pack, object, chunk, or file validation.

## 11. Throughput and contention

The single root is intentionally a repository-level serialization point. It
does not serialize local preparation or capsule transfer; only the final small
CAS is serialized.

At low and moderate contention this maximizes throughput by eliminating lease
and journal traffic. At high contention, different-ref writers can cause CAS
retries. Each retry costs one root GET and one root CAS. The implementation
must measure:

- root CAS attempts and conflicts per committed push;
- uploaded bytes from losing writers;
- time from capsule durability to root commitment;
- root object size and per-key throttling;
- delta depth and checkpoint publication rate.

If production evidence shows sustained root contention, the next design must
choose explicitly between a coordinator that batches root transitions and a
partitioned ref protocol with a transaction marker. Neither is added as a
fallback because both change the authority model and request accounting.

## 12. Byte/request tradeoff

The new priority order is:

1. correctness;
2. foreground request count and sequential latency;
3. aggregate throughput;
4. transferred and stored bytes.

Remote dedup is used only when the client already has authoritative local
knowledge from its pinned base. A cold client uploads uncertain content in
the capsule instead of probing the object store per chunk. This may duplicate
bytes already present in older capsules, but it preserves the request budget
and cannot create missing data.

Checkpointing, repacking, and background dedup may recover storage efficiency
without becoming publication dependencies. Derived compacted capsules are
published only through a root CAS and old capsules remain until normal GC
proves them unreachable.

## 13. Provider contract

Every advertised v2 provider must prove:

- strongly consistent GET after acknowledged PUT;
- atomic create-if-absent;
- atomic update against ETag/version;
- stable version identity sufficient for CAS;
- exact and suffix range reads;
- cryptographic request-checksum validation, or the readback fallback;
- multipart create, part upload, complete, abort, and uncertainty recovery;
- provider timestamps suitable for conservative grace filtering;
- error classification for not found, conflict, authentication, throttling,
  timeout, and indeterminate completion.

A successful basic PUT is not qualification. Endpoint behavior is tested
against the actual S3-compatible service because compatibility labels do not
prove conditional-write or checksum semantics.

## 14. Observability and release gates

### 14.1 Required metrics

- `object_requests_total{operation,phase,outcome}`;
- `object_request_latency_seconds{operation,phase}`;
- `push_request_count` and `push_sequential_waves`;
- `push_capsule_bytes` and `push_redundant_bytes`;
- `push_root_cas_conflicts` and `push_reconciliation_reads`;
- `capsule_read_ranges`, requested bytes, and overfetch bytes;
- checkpoint delta depth, checkpoint construction lag, and forced foreground
  checkpoint count;
- clone/fetch source capsules, response-pack strategy, pack-generation time,
  and cold-clone request count;
- orphan capsules created and collected.

Metrics count transport attempts, including retries. They must never include
object keys, credentials, or repository secrets in labels.

### 14.2 Regression gates

The release must include deterministic tests proving:

- exact three-request clean push on a checksum-qualified fake provider;
- exact four-request clean push on a readback-required provider;
- no HEAD, LIST, lock, admission, heartbeat, journal, or fence operation;
- one capsule for a multi-commit, multi-ref transaction;
- CAS losers cannot publish stale or non-fast-forward refs;
- disjoint ref edits merge without re-uploading their capsules;
- every crash point leaves either the old complete root or the new complete
  root;
- uncertain root CAS is classified from exact transaction identity;
- concurrent normal GC cannot delete base-reachable or recent capsule data;
- force-push resurrection re-embeds data absent from the base root;
- fresh clone at checkpoint generation directly streams only an exact,
  fully-authorized checkpoint catalog;
- fresh clone at maximum delta depth produces one self-contained Git pack;
- incremental fetch and pull transfer wants minus common haves and update the
  expected worktree without exposing hidden objects;
- thin responses omit only client-proven common bases;
- shallow, partial, and lazy fetch return exact Git-compatible selections;
- strict Git fsck, hydrate, and byte-digest comparison succeed after every
  clone/fetch mode;
- corrupt root, footer, index, range, Git object, and file data fail closed.

### 14.3 Live qualification

RustFS is the deterministic race/crash baseline. Every supported hosted
provider then runs isolated-prefix qualification with:

- tiny and multipart pushes;
- warm and cold clients;
- same-ref and disjoint-ref concurrency;
- injected timeouts before and after every mutation;
- fresh full and partial clones;
- concurrent normal GC and exclusive forced GC;
- request, latency, throughput, byte, and integrity reports.

A provider may advertise the three-request path only when its checksum gate
passes. Otherwise it advertises and enforces the four-request path.

## 15. Hard cutover

Protocol v2 has no dual reader, dual writer, fallback, alias, or automatic
translation from v1. The cutover procedure is:

1. stop every writer for the selected repository scope;
2. retain or export the authoritative source repository;
3. install a v2-only Crab release;
4. initialize `v2/root` and publish one verified checkpoint capsule;
5. fresh-clone through v2, run strict Git fsck, hydrate, and compare file
   digests;
6. enable v2 writers;
7. remove v1 data only through a separately reviewed, exact-scope cleanup.

A v2 client encountering v1-only state fails with an explicit unsupported
layout error. A v1 client does not recognize `v2/root` and must not be allowed
to write during or after cutover.

## 16. Rejected alternatives

### 16.1 One versioned mutable repository object

Embedding both new data and refs in a conditional update could reduce the
complete budget to two requests, or one after advertisement. It is rejected
because it depends on portable historical-version reads, makes one object
grow or strands prior data, creates a bandwidth-heavy hot key, and makes
range access, compaction, and GC provider-specific.

### 16.2 One custom transactional service request

A service could receive data and atomically publish refs in one API call. That
is not an object-store request-minimal protocol; it introduces a Crab data
server and moves the multi-object transaction behind that service.

### 16.3 Preserve separate sidecars and batch requests

Concurrent or batched `.pack`, `.idx`, `.rev`, metadata, and receipt writes
reduce elapsed time but not provider request count. Many providers do not
offer atomic heterogeneous batch PUT, and partial batches retain the current
recovery complexity.

### 16.4 Keep foreground locks and GC fences

Leases avoid some speculative work but require acquire, clock, heartbeat, and
release traffic. Root CAS already prevents lost publication, while capsule
self-containment plus grace provides normal-GC safety. Remote leases remain
appropriate only for exceptional maintenance that bypasses those rules.

### 16.5 Probabilistic dedup as omission proof

Bloom or cache hits may be stale or false positive. They may guide redundant
upload avoidance only when followed by an authoritative proof. The
request-minimal path instead uploads uncertain data, because extra bytes are
safe while omitted required bytes violate reconstruction.

## 17. Implementation sequence

1. **In progress:** freeze the v2 root, capsule, embedded Git pack, checkpoint
   pack, footer, locator, checksum, visibility, and error contracts. Root,
   capsule, ref-transaction, and checkpoint-pointer contracts exist; pack,
   locator, and visibility semantics remain incomplete.
2. **In progress:** build a deterministic capsule writer, range reader, and
   corruption corpus. Whole-capsule encode/decode and section authentication
   exist; range reading remains.
3. **Started:** add a transport request observer and executable budgets before
   wiring push. The readback path has an exact four-request unit gate; live
   provider gates remain.
4. **Started:** implement verified-put capability negotiation and mandatory
   readback fallback. Official AWS S3 uses an explicit SHA-256 request checksum;
   unqualified and custom endpoints retain readback. Live provider gates remain.
5. Replace direct push publication with capsule upload plus root CAS.
6. Implement checkpoint-pack passthrough and selected-object response-pack
   generation over capsule-aware locators.
7. Replace clone, fetch, pull, shallow, partial, lazy-object, hydrate, mount,
   fsck, and repository-browsing reads.
8. Implement background checkpoints, bounded delta traversal, and the hard
   publication backpressure at maximum delta depth.
9. Implement fence-free normal GC and root-exclusive forced GC.
10. Qualify RustFS and every hosted provider under failure and concurrency.
11. Perform the explicit hard cutover and delete v1 runtime paths.

Each step must keep one canonical implementation. Temporary development code
may exist on a branch, but the released binary must not retain v1 fallback
paths after the cutover.

## 18. Acceptance decision

The implementation may proceed behind an unreachable development module, but
production wiring and format freeze require these decisions to be closed:

- **Decided:** independent readback is the safe baseline; the three-request
  path is enabled only for a provider/endpoint that passes cryptographic
  verified-PUT qualification;
- **Partly decided:** roots are capped at 8 MiB. Repositories whose complete
  ref map cannot fit require a separately designed protocol and cannot use v2;
- the maximum capsule size before multipart and the multipart part policy;
- **Decided:** checkpoint windows contain at most 500 ref transactions and use
  power-of-two capsule runs. The frontier has no more than eight populated
  levels in that interval, keeping root + checkpoint + frontier reads at ten
  or fewer while amortized qualified push operations remain below four;
- whether native LFS bodies are capsule sections or retain a separately
  counted protocol;
- the exact active-active boundary, which cannot use one object-store root as
  cross-region consensus.

The final acceptance criterion is not merely lower request count. The new
protocol must demonstrate better p50/p95/p99 push latency, aggregate concurrent
throughput, and cold/warm clone latency while passing every integrity, crash,
CAS-race, GC, and reconstruction gate above.
