# Audit LTX latency and sustained publication capacity

| Document intent | Value |
| --- | --- |
| Content type | Design audit and acceptance gates |
| Audience | LTX, runtime, storage, and qualification contributors |
| Scope | Initial baseline `0f3f4f7617a`; follow-up source audit through `e50055c48bb` and the cache-construction change recorded below. Each diagnostic identifies its source separately. Compared with `origin/main` snapshot `de0bb234abc`, not a fresh main qualification. |
| Status | Hydration fetch, sparse registration and persistent-cache construction isolation are implemented. Loaded scale-out cannot assume idle ownership transfer. Demand faults, installation latency, recovery storms, sustained publication and fleet performance remain open. |

[Scaling plan](vfs-ltx-scale-plan.md) · [Recorded measurements](../../crab-ltx/perf/README.md)

The highest-value next experiments remove provider round trips and cache
bookkeeping from request paths. Keep the existing SQLite VFS, exact-root
verification, fencing, stable command receipts, and follower recovery model.
The recorded sub-millisecond sparse capture and roughly 87–139 ms small-root
RustFS preparation come from different harnesses and revisions. They identify
where to investigate; they cannot be subtracted to explain a public action.
Baseline descriptions retain the original finding; implementation paragraphs
identify changes already made. Each measurement record names its source and
harness separately.

## Priority after the implemented changes

| Priority | Remaining gap | First experiment |
| --- | --- | --- |
| P1 | Hydration fetch isolation is implemented; service latency remains unqualified (19) | Measure same-Cell and sibling-Cell tails under arrivals, fragmented fetches and slow page installation |
| P1 | Sparse activation registry isolation is implemented; recovery-storm performance remains unqualified (20) | Concurrent activation under constrained disk IOPS; measure worker occupancy and shared bridge startup |
| P1 | Persistent-cache construction is admitted and its quadratic byte summation is removed; service recovery remains unqualified (22) | Repeat concurrent recovery with slow metadata I/O and measure unrelated foreground latency |
| P1 | Peer admission changes still need load qualification (15) | Measure concurrent hint expiry, activation delay, retained request bytes, and accepted-command cancellation through HTTP |
| P1 | Cross-Cell SQL worker blocking (9) | Background fetch passes the same-worker probe; qualify demand faults, installation, confirmation and cleanup separately |
| P1 | Directory-cache fills still extend reads and occupy shared blocking jobs (18) | Measure remaining cache-install wait and sibling foreground interference after origin admission isolation |
| P1 | Whole-graph compaction and serial publication debt (4–5) | Sustained updates through repeated debt thresholds; compare response rate with publication rate |
| P1 | Buffered compaction still needs sustained-load qualification (17) | Measure async task progress, foreground interference, and publisher drain through repeated compaction boundaries |
| P1 | Cleanup on the SQL worker and decoder memory (10, 13) | Body/footer buffering and unused replica indexes removed; measure remaining index, confirmation time, RSS, and sibling-Cell latency |
| P1 | Execution load is not proven balanced; continued traffic prevents the idle-transfer gate (12) | Separate settled capacity from scale-out during arrivals; record owner/execution distribution in both |
| P1 | Capacity runs do not fault outstanding follower-only acknowledgements (16) | Kill an owner during sustained arrivals with a proven unpublished tail; verify every acknowledged request after takeover |
| P2 | Eager checksum metadata and demand read amplification (3, 8) | First query **and first mutation**, point/random/scan workloads, cold and churned caches |
| P2 | Hydration cache reuse is restored; fragmentation and concurrent duplicate fetches remain (21) | Fixed-root random/scan workloads; count duplicate range bytes, fetch waves and total hydration time |
| P2 | Checksum maintenance differs between fresh and restored Cells (14) | Hold database and changed-page count fixed; compare capture, clean handoff, host I/O calls, and allocations in both states |
| P2 | Checkpoint tail cost and shared maintenance resources (6, 11) | Long update runs with checkpoint, hydration, and compaction interference |

These priorities identify code-supported risks and missing evidence. They do
not rank measured contributions to public p99: current-source fleet phase
measurements are still missing. The best next fix should remove work
from a measured critical path while retaining the existing authority and
durability contracts. Raising concurrency or queue capacity alone does not
meet that criterion.

The [scheduled RustFS measurements at `c12b41ef638`](../../crab-http-server/deploy/cell-issue-fleet/qualification/2026-09-26-scheduled-baseline.md)
now supply a fixed-20-Cell, 3/5/10/20-node series and one 20-node repeat at five
create/read pairs per second. All 1,500 pairs passed without retries. However,
20-node write p95 varied from 347 to 68 ms between runs, execution was spread
across fewer nodes than ingress, and almost every sampled runtime response
used object proof. These observations make placement/phase attribution and
follower-path qualification the next measurement priorities; they do not
establish the dominant bottleneck or qualify later compaction changes.

## Architecture decision after this audit

Keep one SQLite writer per Cell and immutable, verified LTX roots behind the
authority CAS. The best next change is the smallest ownership change that
removes a measured wait or repeated work while preserving those contracts.
This is not yet a verdict that the current PR meets the performance plan.

1. Complete peer admission and same-worker interference qualification first.
   A low-latency local capture cannot compensate for an ingress rejection or
   a worker waiting on another Cell's storage request.
2. First buffer compaction index reads and move bounded merge work off the async
   task. Then make range compaction and its scratch reservation proportional to
   the affected data before adding publication concurrency. Concurrent work must
   never create competing root publishers for one Cell. If publication still
   cannot drain, evaluate a
   bounded batch of consecutive cuts with one covering root and separate,
   stable command receipts.
3. Treat lazy checksum loading as a second design step after measuring the
   existing eager walk. It needs authenticated old-checksum lookup and exact
   aggregate validation; deferring validation alone is not an optimization.
4. Give hydration an asynchronous fetch stage and a short owner-thread install
   stage. Ordinary SQLite demand reads still use synchronous
   [VFS callbacks](https://www.sqlite.org/c3ref/io_methods.html); an asynchronous
   provider thread does not make an in-progress SQLite statement yield its
   SQL worker. Worker reassignment can help idle executors but cannot move a
   connection with an active call. Any broader scheduling change needs a
   separate ownership design and the one-vCPU interference proof.

At each step, compare the same public application action, database size,
payload entropy, durability mode, offered arrivals, and resource profile.
Record successful response latency, failed arrivals, publication drain, and
recovery together. The service gate must include Entity, Shard, Workflow, and
read-model operations through public handles, as well as the issue service.

### Turn the remaining directions into implementation gates

The next changes need explicit bounds and ownership, in addition to faster
microbenchmarks. These are proposed acceptance gates, not achieved SLOs.

| Order | Change boundary | Required result |
| --- | --- | --- |
| 1 | HTTP action → runtime receipt → winning proof → client response | Correlate submission ID, attempt ID, Cell, owner, commit sequence, and actual HTTP acknowledgement in traces/raw samples. Keep IDs out of metric labels. Report queue, SQL/capture, proof and confirmation on the same action; do not add unrelated histogram percentiles. |
| 2 | Directory read and cache installation | Release origin admission after bounded transfer/verification. Then evaluate returning verified bytes before a bounded cache fill completes. A slow or canceled cache fill must not retain network admission, escape byte/job/disk accounting, or make cache contents authoritative. |
| 3 | Hydration and SQL ownership | Fetch authenticated pages asynchronously, then install a bounded batch on the owning worker, checking that owner writes have not superseded those pages. Give foreground work priority between batches. An active SQLite demand read still blocks its worker; retain that limit until a separate ownership solution passes same-worker tests. |
| 4 | Compaction and root publication | Reuse unchanged authenticated directory branches. With the selected range held fixed, metadata work and scratch should scale with affected locators/branches rather than all descriptors and database bytes. Preserve newest-wins, truncate/regrow and exact predecessor checks; benchmark repeated 31/32/33-segment crossings. |
| 5 | Checksum state | Use bounded authenticated checksum blocks for old-value lookup and transactional updates. With changed pages held fixed, a fresh Cell must not copy a database-sized array every cut. Lazy activation must retain aggregate verification, including deleted suffixes and later mutations. |
| 6 | Sustained service and recovery | At fixed Cell count and offered arrivals, measure acknowledgement rate, publication rate, retained bytes, oldest unpublished age, rejection rate and latency together. Run long enough to cross repeated compaction/checkpoint cycles. Fault a proven follower-only acknowledged tail during arrivals and verify every acknowledged result after owner/local-data loss. |

**Action-attribution implementation:** the shared HTTP submission validator
records the stable application input under the existing server request span.
The public typed client records its runtime attempt and committed receipt
before the issue/label/status/check output adapters return their payload.
The owner carries Cell, incarnation, request ID and owner session through actor
and worker queues, capture, winning proof and final successful reply. IDs live
in traces, never metric labels. Effects share worker/actor tracing under their
effect identity; the issue-load join qualifies only command writes.

The scheduled fleet runner retains every HTTP attempt and the successful
response's `x-request-id`. It collects each node's log before owner loss and
joins every acknowledged write to exactly one matching owner response and
commit sequence. Missing phases, conflicting owners, mismatched receipts or
ambiguous duplicate responses fail attribution. The report records actual
write owners and forwarded writes; the previous pre-load-owner estimate was
removed. A stored runtime result has source `Recorded` and does not invent a
new capture or proof. A new runtime attempt deduplicated by the application can
still commit a new runtime receipt; its stable submission ID remains separate.

The real RustFS/mTLS HTTP test passed after owner takeover and Git readback;
its actual text-formatted logs also passed the fleet join CLI. One acknowledged
issue write measured 165.890 ms at the client, 164.083 ms to HTTP response
readiness, 72.339 ms in the typed invocation, 4.065 ms on the SQL worker
(including 1.653 ms capture), and 27.227 ms waiting in the proof task. This is
one diagnostic action on a debug build, not a percentile or an isolated
bottleneck measurement. Artifacts beneath the checkout's external target are
`action-trace-rustfs-formatted.log`, `action-trace-rustfs.samples.jsonl`, and
`action-trace-rustfs.actions.jsonl`.

The timings overlap: capture is inside worker time; worker and proof work sit
inside broader request lifetimes. HTTP response readiness precedes client body
receipt. Proof wait starts when the proof task runs and excludes earlier node-
log submission and actor scheduling. Authentication/routing/activation, query
phases, individual provider attempts, and proof submission need further
attribution before a complete latency decomposition. Replaying a shared-process
test log proves the correlation fields and parser, not cross-container collection.
Current-source Compose load, trace-overhead comparison, and saturation curves
remain required. See the [action trace runbook](../../crab-http-server/REFERENCE.md#attribute-acknowledged-cell-writes).

Keep checkpoint execution serialized with the managed writer when exploring
background work. The workspace pins `rusqlite` 0.34.0 / `libsqlite3-sys` 0.32.0;
the bundled header reports SQLite 3.49.1. SQLite's
[WAL-reset guidance](https://www.sqlite.org/wal.html)
identifies a write/checkpoint race fixed in 3.51.3 and selected backports.
The current exclusive `Db` contract and serialized worker do not establish
that race is reachable here. Moving checkpoints onto a competing connection
requires dependency qualification and a new capture-order proof before any
performance claim. Offloading file cleanup is a different operation.

## Findings in execution order

### 1. Small LTX bodies pay the multipart protocol cost

**Confirmed at audited revision:** [native segment upload](../../crab-ltx/src/replica/upload.rs)
calls [upload_source](../../crab-ltx/src/replica/compaction/scratch.rs), which
always calls `Store::put_multipart_source_retry`. The
[storage implementation](../../crab-storage/src/store.rs) starts multipart
even when the source fits in one part. The pinned `object_store` 0.14.1 S3
implementation separately creates, uploads, and completes the upload; 0.14.2
is pinned by the standalone cost harness. This agrees with the
[S3 multipart protocol](https://docs.aws.amazon.com/AmazonS3/latest/userguide/mpuoverview.html).
The publication counter counts the completed object once, not these requests.

**Change to evaluate:** read and verify a small, admitted source into a bounded
buffer and use the existing create-only `Store::put`. Keep multipart for larger
sources. Locate the size decision at the storage transfer boundary after
checking its other callers; avoid separate policies in capture and compaction.
Retain source length/digest verification, exact retry bytes, cancellation,
same-content reconciliation, and conflict refusal.

**Gate:** one successful small body uses one PUT without multipart initiation;
count provider attempts, including retries. Test changed/truncated sources,
lost responses, existing same/different content, and both native and compaction
uploads. Compare public action p95/p99 and publisher drain rate over RustFS.
The current five-object small-write count is not five HTTP requests.

**Implementation:** native, compacted, and bundled bodies now share a private
LTX transfer function. Sources up to 256 KiB are length/digest verified into
one buffer under the existing host I/O permit, then sent with `Store::put_exact`.
That method retains exact staging paths, create-only writes, same-content
reconciliation after an uncertain response, and different-content refusal.
Larger sources retain multipart streaming. The generic storage multipart API
has other overwrite, staging, and cancellation callers; it is unchanged.
Recovery manifests also retain their existing transfer path.

Focused tests count transfer calls and restore native, compacted, and bundled
roots byte-for-byte for both sizes. They inject a lost response after a
successful create and reject wrong lengths, changed digests, truncated sources,
and conflicting existing content. These establish transfer semantics, not
public-action latency or sustainable publication capacity.

The first seven-run RustFS comparison is
[recorded with its raw artifact location](../../crab-ltx/perf/README.md#small-body-transfer-experiment).
Median run p50 moved from 6.64 to 6.16 ms, but p95 rose from 10.95 to
12.14 ms and unchanged local phases slowed. Latency qualification remains
open. The real HTTP/peer RustFS test passed mutations and takeover restore.

### 2. Persistent directory-cache hits perform durable index writes

**Confirmed at audited revision:** [DirectoryCache::get](../../crab-ltx/src/environment/directory_cache.rs)
calls `persist_index` after a valid hit and after a missing file. That method
clones and serializes the entire entry map, writes a new file, syncs it, and
renames it. A hit changes in-memory recency, but the serialized index contains
only keys and lengths. The hit therefore persists unchanged content. This
path runs after a memory-cache miss in
[read_node](../../crab-ltx/src/replica/directory.rs); it does not affect a pure
memory hit. The runtime installs this cache during
[Cell acquisition](../src/cell/actor/acquire.rs).

**Change to evaluate:** eliminate index persistence on pure hits and misses
that remove no indexed entry. Retain durable cache installation and actual
membership updates initially. Keep recency maintenance bounded; its current
queue scan also grows with cache entry count.

**Gate:** a verified disk-cache hit makes zero writes, renames, and syncs;
an absent unindexed key also makes zero writes. Preserve restart, corruption,
symlink, concurrent-fill, eviction, and disk-accounting tests. Measure with
the memory cache churned, a nearly full disk index, and concurrent Cells.
This removes redundant work without changing cache durability policy.

**Implementation:** verified hits now update only memory recency; absent keys
persist the index only when indexed membership was actually removed. Durable
installation, invalidation, and eviction retain their writes. A regression
test reproduced the old hit write and now proves neither a hit nor an
unindexed miss creates an index file. Existing restart, corrupt-entry, symlink,
and accounting tests pass. Full-cache concurrent latency remains unmeasured.

**Remaining cost:** a fill still persists the entire membership index, and a
hit scans the recency queue with `retain` while holding the state mutex. The
entry cap is 16,384. Removing hit fsyncs does not establish constant-time hit
cost or cheap cold fills. Compare hit/fill latency as entry count grows, plus
concurrent activation that churns the process-wide 8 MiB directory cache.
Evaluate bounded recency bookkeeping and batched/reconstructible membership
persistence only with restart, corruption, and disk-accounting proof.

### 3. Sparse writable activation still reads the complete checksum directory

**Confirmed:** [prepare_writable](../../crab-ltx/src/replica.rs) awaits
[load_checksums](../../crab-ltx/src/replica/directory/checksums.rs). That function visits
every directory node, authenticates every page entry, writes
eight checksum bytes per database page, and syncs the checksum file before
opening the writer. LTX page bodies are lazy; this metadata walk is eager.
At 4 KiB pages a 10 GiB database alone needs a 20 MiB checksum file, excluding
directory transfer and validation. Tiny bootstrap Cells hide this cost.
The directory format stores 88 bytes per page locator, plus a 32-byte header
per 256-entry leaf. For a dense 1 GiB database at 4 KiB pages, that is about
22 MiB of leaf metadata to read/validate on a cold walk, plus a 2 MiB checksum
file and internal nodes. These are format-derived sizes, not resident-memory
requirements or a supported database-size claim. Measure directory transfer
as well as sidecar bytes when setting recovery targets.
At the audited revision, this async path directly called synchronous filesystem
writes and syncs, including the final checksum barrier, instead of dispatching
them through the host's blocking executor. Slow local storage could therefore
stall its Tokio worker as well as delaying activation; its fleet latency impact
remains unmeasured.

**Change to evaluate:** first remove finding 2's cache overhead and overlap
independent authenticated node reads within existing host admission, retaining
ordered validation/output. Consider lazy checksum chunks only if this measured
walk still prevents the recovery target. That larger change must preserve the
old checksum for every overwritten/truncated page and the final root checksum.
Move checksum writes and barriers through admitted blocking work, retaining
file and reservation ownership until dispatched work finishes.

**Gate:** measure first query and first mutation separately for fixed data
sizes, empty/warm caches, and simultaneous owner loss. Count directory requests,
checksum bytes, local syncs, admission wait, RSS, and time to serve. Exact-root
corruption, canceled activation, fresh destination, and checksum-link tests
must remain intact. A fast first page fault does not prove fast activation.
Inject slow filesystem writes/syncs and verify unrelated task progress, bounded
blocking admission, and cleanup when activation is canceled or returns an error.

**Implementation:** checksum-file creation, bounded 64 KiB writes, final sync,
parent sync, and metadata validation now use `Host::run`. The file owner moves
with each dispatched job and retains dirty admission. Cancellation schedules
cleanup through the same blocking-job ceiling; successful delivery disarms
cleanup only after the validated checksum handle reaches its caller. Normal
errors await cleanup before returning. Cleanup remains best effort when the
filesystem or executor fails, and requires the Tokio runtime to stay alive.
This follows the [runtime task contract](https://docs.rs/tokio/1.53.1/tokio/runtime/struct.Handle.html#method.spawn).

The regression first failed on the old async-thread filesystem call, then
passed with a 40 MB database, disk directory cache, and one blocking-job slot.
Fault tests pause creation, writes, both sync barriers, and final metadata;
unrelated async work progresses, cancellation retains both admissions through
paused cleanup, and the same destination can be retried and queried. Error
injection preserves pre-existing destinations. Exact-root, sparse publication,
directory, and process-kill recovery tests pass. Activation percentiles and
fleet interference remain open.
The real RustFS HTTP/peer test also passes application mutations, owner
takeover, restored collaboration state, and Git reads with this path.
Full-image restore is a separate sibling: bulk writes/syncs already use host
jobs, but initial file setup and scratch cleanup still need the same audit.
Compaction needs that audit too: its scratch creation, final index-file opens,
and `MergedEntries` iteration perform filesystem work from async preparation.
The iterator is consumed by `directory::initial::build_and_upload`, so moving
only the initial opens would leave synchronous index reads on the async path.

Sibling leaf reads now overlap through an ordered stream capped at eight,
sharing existing host I/O slots. Internal branches stay depth first so sibling
prefetch cannot reorder coverage or aggregate validation. With ten leaves and
100 ms injected delay per GET, the previous scan took 1,000 ms even with four
slots; the changed scan takes 300 ms. One slot takes 1,000 ms and sixteen slots
still take 200 ms, proving the eight-read ceiling in that fixture. These are
virtual-time scheduling results, not RustFS performance percentiles.

A 40 MB database with 512-byte pages exercises two parent levels. A separate
late-leaf corruption test refuses activation, releases I/O admission, removes
its checksum file, and permits retry after repairing the object. The 26
exact-root/sparse tests and four activation tests pass. This change applies to
writable activation only; selected-page lookup and exhaustive retention
inventory retain their existing traversal and verification contracts.

The [real RustFS activation probe](../../crab-ltx/perf/README.md#sparse-activation-over-real-rustfs)
now separates root open, checksum preparation, writable open, and first query.
On a 256 MiB source, three cold-metadata samples per admission setting measured
checksum preparation medians of 1,057 ms with one I/O slot, 420 ms with four,
and 295 ms with eight. All settings fetched 259 objects and 5,797,768 bytes.
Reused metadata required zero origin reads but still spent 57–119 ms in this
phase. This supports bounded read overlap for cold metadata; it does not
attribute the remaining local cost or qualify end-to-end tails. The probe
also fixes an earlier timer that included compaction and a second restore in
the reported restore duration.

### 4. Range compaction can do whole-graph metadata work on the publication lane

**Confirmed:** [compaction::prepare](../../crab-ltx/src/replica/compaction.rs)
spools indexes for **all** descriptors, although it spools bodies only for the
selected range. It then merges the final descriptor set and rebuilds the
complete directory. [CellPublisher](../src/publication.rs) checks compaction
after eight appends during quiet periods and forces debt handling before an
append projected to reach 32 segments. That work retains the serialized
publisher token; it can delay following roots even after follower responses.

The admission estimate also grows with the whole database:
[compaction_scratch_bytes](../../crab-ltx/src/replica.rs) reserves twice the
logical database size plus 64 MiB, every descriptor index, and the selected
compressed bodies. The first term comes from
[full_job_scratch_bytes](../../crab-ltx/src/recovery.rs). A 1 GiB database thus
requires over 2 GiB of scratch admission even for a small selected range.
This is disk reservation, not resident memory or measured peak disk use.
Optimizing the merge alone leaves this admission floor unchanged.

**Change to evaluate:** reuse authenticated unchanged directory branches and
update locators only where selected segments still supply the current page.
Measure before making compaction concurrent with publication: any such change
needs an exact predecessor check and must discard or safely rebase stale work.
Derive scratch admission from the bounded range algorithm in the same change;
include codec scratch, worst-case output expansion, indexes, and cancellation
lifetime. Keep the current conservative bound until that proof exists.

**Gate:** run updates as well as inserts on a large base; cross repeated
8-segment promotion and 31/32/33-segment pressure boundaries. Record all-index
bytes, directory rewrites, scratch peak, root lag, foreground p99, and restored
byte equality. Raw library tests around 96/97 descriptors cover descriptor-page
boundaries; they do not substitute for the runtime's earlier debt threshold.

### 5. Early follower responses do not establish sustainable write throughput

**Confirmed at audited revision:** [start_publication](../src/cell/actor/requests.rs) removes one
publisher from the active Cell and publishes one queued command at a time.
`prove_command` races external proofs and discards their source before final
worker confirmation. Completed-proof counters cannot identify the proof that
released each successful action. The existing byte admission bounds backlog;
it does not make an arrival rate above publication capacity sustainable.

**Change to evaluate:** record one response winner plus confirmation time and
publisher queue age. After findings 1–4, evaluate bounded root coalescing only
if a hot Cell still cannot drain. Preserve each command's receipt, outcome,
effect ordering, and replay coverage even if one root covers several commands.

**Gate:** offered-rate sweeps must hold long enough for compaction and backlog
to settle. Accepted commands/s, published commands/s, root lag, retained bytes,
and rejections must be reported together. Owner loss with follower-only tails,
duplicate delivery, ambiguous publication, and rollout must still work through
public application handles. Increasing queue limits is not a throughput fix.

**Instrumentation:** the command/effect reply boundary now reports one source
(`fleet`, `object`, or `recorded`), admitted-enqueue-to-response time, and final
SQL worker confirmation time. It records only a successful runtime delivery;
failed results and abandoned receivers add no winner. A later object proof
does not add another response. Structured traces include Cell and commit
sequence; Prometheus labels contain only the finite source. Queries,
migrations, and transport have separate boundaries and are excluded. This
closes response-source ambiguity; sustained drain and full phase attribution
remain unqualified.

### 6. Bounded I/O does not yet prove foreground latency isolation

**Confirmed:** [Host](../../crab-ltx/src/environment/host.rs) shares I/O permits
across page faults, uploads, and compaction. Compaction and restore also share
recovery admission. [Paged I/O](../../crab-ltx/src/paged_io.rs) has 32 active
jobs, a 256-request queue, an 8 MiB cache, and a per-view gate. These provide
bounds; no latency-priority guarantee follows from those bounds. Directory
origin reads now release their I/O permit before admitted disk-cache insertion;
the reader still waits for that insertion and its blocking job remains shared.

**Change to evaluate:** measure permit wait and background interference before
changing concurrency. Release provider permits when transfer/verification no
longer needs them; if measurements justify scheduling changes, reserve progress
for foreground reads and durability while preventing compaction starvation.

**Gate:** under 1 vCPU/1 GiB per node, overlap cold activation, hydration,
compaction, and writes. Report foreground p99, queue deadlines/rejections,
provider concurrency, and maintenance debt. More concurrent requests must not
silently exceed memory or provider budgets. Test fragmented page spans as well
as the existing contiguous hydration case.

The sparse bridge adds another shared execution boundary: `Driver::new` uses a
current-thread Tokio runtime. `read_run` decodes and checksums its fetched span
inline before returning to that runtime. Consequently, 32 active async jobs
are not 32 parallel decoders. Compare decode time and driver scheduling delay
before dispatching bounded decode jobs; full restore already decodes its
windows inside `Host::run`. Tokio's
[current-thread and fairness contracts](https://docs.rs/tokio/1.53.1/tokio/runtime/index.html)
require bounded task polling time. No measured decoder bottleneck is claimed.

### 7. Qualification needs a less favorable payload and arrival model

**Confirmed at audited revision:** the [cost harness](../../crab-ltx/perf/replica-cost/src/main.rs)
generates `(command * 131 + index) % 251`, repeating every 251 bytes. It is
compressible despite its source comment. Its loop calls `CellReplica::prepare`
directly: no runtime authority CAS, follower race, or scheduled compaction.
The [Compose load generator](../../crab-http-server/deploy/cell-issue-fleet/load.py)
uses one sequential write/read lane per Cell, with the Cell count equal to
node count. Slow responses reduce offered traffic. Its retry-inclusive logical
latencies are useful, but cannot establish an arrival-rate saturation curve.

**Change to evaluate:** retain the historical fixture and label its entropy.
Add seeded high-entropy bytes, structured application data, update/delete churn,
and variable database sizes. Hold Cell count and offered rate fixed while
changing nodes, then independently vary skew and load. Add scheduled arrivals
with a bounded outstanding limit; report rejected/late arrivals instead of
silently slowing the generator. Bind source, image, target architecture,
filesystem, SQLite and provider dependency versions to each report.

**Gate:** retain raw samples and repeated runs with enough completed operations
to study tails. Twenty-eight preparation samples cannot establish p99; that
nearest-rank p99 is the maximum. Keep microbenchmark, public action, Compose,
and multi-host results separately labeled.

**Instrumentation:** the cost harness now labels the periodic payload, offers
deterministic command-seeded random bytes, and records root-preparation calls,
outcomes, bytes, and durations through the existing storage observer. The
[RustFS smoke evidence](../../crab-ltx/perf/README.md#backend-calls-and-payload-entropy)
shows two HEADs and five PUTs per small prepared root; the larger random body
uses multipart. Provider-internal retries remain opaque.

The Compose generator now keeps a fixed Cell count across node stages. Its
load runner schedules create/read pairs independently of completion, bounds
in-flight work, and records client-capacity rejections and missed scheduling
intervals. Uniform, hot, and skewed targeting retain arrival indices and stable
write IDs in raw samples. Runtime metrics and container resources are sampled
during load; post-load uncovered bytes must drain on every node. Failed runs
retain reports, and acknowledged readback mismatches stop new arrivals.
Controllable HTTP tests cover slow responses, lost responses, and delayed
scheduling. Sustained offered-rate curves, update/delete churn, and full action
phase attribution still need qualification against the current native image.

### 8. Sparse point faults and bulk hydration use the same read-ahead window

**Confirmed:** [paged_io::fetch](../../crab-ltx/src/paged_io.rs) asks for up to
64 pages for every cache miss, for both `Sparse` and `Hydrating` origins.
[CellPagedDatabase::read_run](../../crab-ltx/src/replica.rs) selects the first
same-object span in that window, fetches it, and validates every returned frame
before the requested page is delivered. Extra pages enter the shared 8 MiB
view-keyed cache; only pages demanded by SQLite become materialized VFS pages.

In the 32 MiB RustFS probe, the first `SELECT length(value) ... WHERE rowid = 1`
made two range reads totaling 524,994 bytes. The whole activation, including
open, materialized just four 4 KiB pages. The 256 MiB probe fetched 265,332 bytes
for that query, reflecting a different segment layout. This is observable
read amplification, not proof that a smaller window universally lowers latency:
read-ahead can avoid later requests during scans and hydration.

**Change to evaluate:** distinguish demand access from bulk progress using the
existing origin classification. Compare demand-only reads, bounded adaptive
read-ahead after sequential access, and the current window. Keep hydration
coalescing, the shared memory ceiling, deadlines, and authentication intact.
Any asynchronous prefetch must retain admission until completion and must not
publish unverified bytes or occupy all foreground slots.

**Gate:** compare point lookups, random reads, sequential scans, and hydration
on both contiguous and fragmented roots. Record requested/decoded/consumed
pages, useful prefetch hits, wasted bytes, origin calls, CPU, and p50/p95/p99
under concurrent Cells. An improvement in point-read bytes must not silently
regress scan throughput or starve durable publication. The existing contiguous
hydration test proves coalescing correctness; it does not establish this tradeoff.

**Additional source gap at `7d3dd9232b0`:** `read_run` computes all directory
spans for its window, then consumes only `.next()`. With fragmented locators,
a 64-page window can produce 64 one-page spans while the call fetches just
the first. Later faults repeat directory parsing and span allocation for
overlapping windows; the byte cache avoids downloads of directory nodes but
does not cache their parsed, context-validated entries. Full restore already
consumes all spans with bounded concurrent fetches in `read_restore_window`.
Evaluate stopping demand lookup at the first useful span and giving bulk
hydration bounded multi-span progress. Keep verification scoped to the exact
root/extents; a digest-only parsed cache cannot silently reuse validation
against a different root. Count parsed entries, discarded spans, fetch waves
and consumed pages on alternating-object roots before selecting a policy.
This is a source-supported opportunity, not a measured latency contribution.

### 9. One cold Cell can block unrelated Cells on its SQL worker

**Confirmed:** [SqlWorkerPool](../src/cell/worker.rs) assigns a Cell to a fixed
worker using its ID modulo worker count. The
[worker loop](../src/cell/worker/run.rs) executes one synchronous operation at
a time. A sparse VFS read waits in `paged_io::receive` for its provider result;
the dedicated I/O thread keeps the provider progressing but cannot run another
Cell's SQLite operation on the blocked SQL worker. CPU-derived sizing can
select one worker for the requested 1-vCPU node profile.

[Background hydration](../src/cell/actor/lifecycle/background.rs) checks the
selected Cell's queue, then sends up to 64 pages through the same worker and
the same global worker-job semaphore as foreground queries. An idle Cell does
not imply an idle worker. On a multiworker node, jobs queued for one busy shard
can also occupy the global admission permits while another shard has capacity.
`confirm_durable` and `confirm_published` use that worker, so this interference
can extend durable response time as well as query time.

**Change to evaluate:** measure worker admission, shard queue, SQL execution,
and sparse-provider wait independently. Make maintenance admission aware of
worker foreground demand. Prefer admitted asynchronous fetch followed by short
owner-thread installation for hydration. If demand faults still dominate,
evaluate bounded reassignment of idle Cell executors to available workers;
preserve exclusive connection ownership, per-Cell order, cancellation, and
fencing. Merely adding provider I/O slots cannot resolve a blocked SQL shard.

**Gate:** one cold/fragmented Cell plus one resident Cell on the same worker,
then different workers; slow origin, slow local sync, canceled waiter, and
background hydration cases. Measure resident p99 and confirmation latency,
including waits before worker dispatch. The existing
`sparse_fault_pool_progresses_under_saturated_sql_workers` test places two
Cells on **different** workers and proves I/O progress, not same-worker latency
isolation. Hydration cancellation tests protect admission but do not establish
foreground latency. This scheduling behavior also exists on the main snapshot.

**Implementation:** worker-job admission now has one permit for each fixed SQL
worker. A queued job waits for its own worker before reserving a node job slot;
it cannot consume another worker's capacity. Dispatched work retains its permit
and ledger reservation through completion even when its caller is canceled.
Shutdown closes every admission queue. Cell assignment, thread count, actor
ordering, lifecycle messages, and publication-confirmation messages are unchanged.

A public worker regression holds one native operation, queues another Cell on
that worker, and invokes a third Cell on the idle worker. The old global gate
failed its one-second completion bound twice; the per-worker gate passes, and
canceling the queued request leaves no mutation behind. A second fixture uses
real sparse SQLite with delayed object-store reads during hydration and a fully
materialized Cell on the other worker. It also fails on the previous gate and
passes with per-worker admission. This separates admission from SQLite and
storage progress; these controlled delays are not public-action percentiles.
Seventeen focused worker, hydration, and public three-node application tests
pass, as does runtime all-target Clippy. The real RustFS HTTP regression also
passes through remote execution, owner loss, restored collaboration state, and
Git clone/tag reads. These checks protect behavior; they do not replace the
current-source fleet curves.

**Asynchronous hydration implementation:** preparation selects at most 64
missing pages on the owner worker; fetch releases worker admission; installation
returns authenticated bytes and their retained-byte reservation to that worker.
Installation checks activation identity and skips pages superseded by owner
writes or truncation. Dropping a fetched batch cannot advance the cursor.

The strengthened worker regression fails with the committed synchronous
hydration path and passes with the initial draft. A real RustFS diagnostic adds 500 ms
to each GET and queries fully resident Cells on the same and another worker:

| Diagnostic | Synchronous runtime | Asynchronous draft |
| --- | ---: | ---: |
| Same-worker resident query | 1,026.685 ms | 0.053 ms |
| Other-worker resident query | 0.163 ms | 0.060 ms |
| Hydration plus queries | 1,027.172 ms | 1,526.698 ms |
| Origin read operations during hydration | 2 | 3 |

These are single debug-build, injected-delay diagnostics against the local
RustFS fixture, not p95/p99 or a service SLO. The before run temporarily used
the committed executor/worker dispatch with the same current test fixture;
the draft source was restored afterwards. They establish the sibling-Cell
wait and its removal, not faster hydration: the draft makes an additional
origin read in this fixture (finding 21). The workload creates a fresh random
database each run, so byte-for-byte traffic attribution needs a fixed-root
comparison. Raw logs are `worker-interference-rustfs-before-matched.log` and
`audit-hydration-rustfs.log` beneath the checkout's external target directory.

Demand faults remain synchronous, and installation, confirmation and cleanup
still execute on the worker. The initial split needed additional same-Cell
isolation and cache reuse; findings 19 and 21 record those follow-ups and their
tests. The final RustFS diagnostic (`hydration-rustfs-final.log`) measured
0.129 ms for the same-worker resident query, 0.119 ms on the other worker,
and 1,010.155 ms for hydration plus queries, with two origin reads. This remains
a single injected-delay diagnostic. It does not establish service percentiles
or a throughput change across different random database fixtures.

### 10. Published-cut cleanup occupies the SQL worker after durability

**Confirmed at audited revision:** [CellExecutor::confirm_published](../src/cell/executor.rs) calls
[Db::prune_captured](../../crab-ltx/src/db.rs) on the SQL worker. Its
`prune_retained` implementation reads the entire selected file into a `Vec`,
hashes and decodes it through
[verify_segment](../../crab-ltx/src/recovery.rs), then deletes it and reconciles
disk accounting. The decoder avoids retaining every decoded page, but the
compressed input remains fully buffered. Default library limits permit a
512 MiB LTX file; these limits explicitly are not an RSS quota.

On the object-only actor path, the publication proof channel is completed
after this cleanup. With node-log durability, an external proof can arrive
earlier, but the final `confirm_durable` still waits for its SQL worker. Thus
cleanup can delay a response or later work after external durability exists.
This is separate from the capture-time reinspection already removed and
[measured](../../crab-ltx/perf/README.md#large-sparse-checkpoint-capture-2026-09-25).
The same cleanup implementation exists on the main snapshot.

**Change to evaluate:** first replace full-buffer verification with admitted,
streaming verification using the existing decoder contract. Then evaluate
moving exact-file cleanup out of SQL execution. Keep retained disk accounting
and owned file identity until deletion finishes; bound cleanup debt. Separating
durable sequence advancement from file reclamation requires explicit handling
of cleanup failure, shutdown, duplicate confirmation, and canceled waiters.
Do not simply remove verification or release reservations when dispatch starts.

**Gate:** small cuts, large incompressible cuts, and full-image cuts under the
1 GiB profile. Record cleanup bytes, temporary RSS, worker occupancy, proof-to-
response delay, and sibling-Cell p99. Preserve
`captured_pruning_retains_accounting_after_io_failure`,
`published_deferred_capture_is_pruned_without_a_local_durability_barrier`, and
`published_root_survives_local_prune_failure_without_replaying_sql`. These
already distinguish a published root from failed local reclamation; they do
not bound reclamation latency or memory.

**Implementation:** cleanup now opens the selected file once and verifies it
through a 64 KiB buffered reader. The canonical stream verifier checks header
limits, full page/index/trailer structure, exact length, metadata, and BLAKE3
before deletion. Bundle and node-frame callers retain their byte-buffer
length/digest rejection before the same structural verifier. Commands,
migrations, and bootstrap all use this cleanup path; no authority or sequence
transition moved. A read or unlink failure still retains unfinished accounting.

The new public-`Db` regression fails on the old whole-file transfer and passes
with streaming; truncation, extension, corruption, and substitution with another
valid cut preserve files and reservations until a successful retry. A local
[RustFS comparison](../../crab-ltx/perf/README.md#streaming-published-cut-cleanup-2026-09-26)
observed 194.7 to 153.7 ms median cleanup for the largest 50.8 MB batch across
three processes, with an unchanged pooled 0.161 ms small-cut median. This is
microbenchmark evidence. Cleanup still blocks the SQL worker and the decoder
still allocates indexes; finding 13 prevents interpreting this as constant
memory or completed foreground-latency qualification.

### 11. Long-run checkpoint tails need their own qualification

**Confirmed:** [checkpoint_if_needed](../../crab-ltx/src/capture/checkpoint.rs)
runs inside capture. Passive work is triggered by appended frames or elapsed
time; emergency truncation uses the original logical WAL size and a threshold
at least as large as the database for larger databases. A truncate restart
captures a full boundary image. The relative threshold avoids repeated
database-sized captures on every large insert, but the eventual boundary
capture still runs synchronously before that command's cuts are returned.

**Change to evaluate:** attribute WAL bytes, checkpoint mode/restart, full-image
cut bytes, and their downstream upload/cleanup cost to the triggering action.
Explore scheduling safe checkpoint work during available worker time only after
measuring the remaining tails. Keep the writer barrier, exact sealed boundary,
and WAL restart detection. SQLite's
[checkpoint contract](https://www.sqlite.org/wal.html#performance_considerations)
explains why checkpoints involve extra I/O; Crab's managed checkpoint policy,
rather than SQLite's default autocheckpoint threshold, controls this path.

**Gate:** repeated updates of a fixed-size database, inserts, deletes/truncation,
and concurrent Cells over multiple checkpoint cycles. Include p99/max and the
largest retained cut, not just steady small-cut medians. Existing sparse
checkpoint measurements use a different harness and short runs; they cannot
establish public action tails or memory headroom during a full-image cut.
This path is unchanged from the main snapshot.

Before moving checkpoints onto a concurrent connection, check the SQLite
dependency too. The workspace enables `rusqlite`'s bundled feature; the locked
`libsqlite3-sys` 0.32.0 source contains SQLite 3.49.1. SQLite's official
[WAL-reset advisory](https://www.sqlite.org/wal.html) identifies a
write/checkpoint race in that version range and names fixed releases. The
current exclusive worker and managed checkpoint barrier serialize this path;
this audit has not reproduced that upstream race in Crab. Any proposal to
overlap those operations must qualify a fixed SQLite build first, then prove
the capture barrier and restart handling. Record the linked SQLite version in
the evidence; a Rust crate version alone does not identify it.

### 12. Even ingress and Cell targeting do not prove even owner execution

**Confirmed at audited revision:** [run_stage](../../crab-http-server/deploy/cell-issue-fleet/qualify.py)
checks live sessions, reads existing Cells, and records their owners. The
[load runner](../../crab-http-server/deploy/cell-issue-fleet/load.py) checks a
70–130% ingress split and uniform offered Cell targets. It does not enforce
owner balance or record the executing owner for each action. Its forwarded
count compares ingress with a pre-load owner observation, which can become
stale during movement. The older main runner also lacked this execution proof.

A real RustFS functional rerun grew 20 fixed Cells from three to five nodes.
All five owned Cells, but their counts were **6, 5, 5, 1, 3**, not four each.
This observation demonstrates the measurement distinction; it does not prove
placement cannot converge. It used the earlier local server image
`4ecf6e3e6e83`, whose source revision is unavailable, and cannot qualify the
current runtime's placement or throughput. Raw owner maps and container data
are retained in `five-startup.json` beside the scheduled harness smoke report.

**Change to evaluate:** record owner/epoch and execution counts over the
measurement interval. For uniform scale comparisons, require a documented
placement settling criterion and report workload-weighted execution imbalance;
retain skew as a separate intentional workload. Preserve fixed data size and
Cell count across stages. One Cell per node is too coarse for meaningful
load-balance behavior; test several Cells per node and a hot Cell separately.

**Gate:** distinguish healthy containers, ingress distribution, Cell target
distribution, owner distribution, and actual execution distribution. Also record
the Docker VM's physical CPU/memory and host contention: twenty 1-vCPU limits
on an 8-vCPU VM are an oversubscribed topology, not twenty independent CPUs.
Require current-source images, repeated sustained runs, and isolated multi-host
failure domains before choosing supported limits. Run Entity, Shard, Workflow,
and read-model actions through public application handles as well as this issue
service; issue creation alone does not exercise those service compositions.

At the audited revision there was also a provenance gap: both `qualify.py` and
`load.py` record the harness checkout's `git rev-parse HEAD` independently of
the image ID. `--skip-build` does not verify that the selected image was built
from that revision, and HEAD does not describe dirty build input. Require a
clean build or retained source-tree digest, and verify image provenance before
calling a comparison current-source evidence. Distinguish an OCI manifest
digest from its configuration digest when comparing Docker engines.

**Implementation:** qualification now requires a clean checkout and builds
from its committed Git archive, so ignored files or edits during the build do
not alter the attributed source. Local and CI source builds set the same
revision label as release builds. The runner rejects a missing or mismatched
label before startup, pins every server service and release bootstrap to the
inspected image ID, retains a tag for that image, and checks each running node.
Schema 3 load reports separate server source/platform/image from harness source.
Labels remain producer metadata; imported images still need their CI source
and checksum receipts.

The wrong-source regression fails on the previous runner and passes after the
change. Nine scheduler/provenance cases pass. A real Docker fixture builds from
a committed archive while its working file differs, extracts the committed
bytes from the image, moves the build tag, and verifies the retained image and
wrong-source refusal. All Compose profiles validate. The first Docker trial
exposed image-index disappearance after retagging; retaining the qualification
tag fixes that observed failure. This verifies evidence binding, not service
latency; current-source fleet curves remain open.

**Attribution follow-up:** schema 4 now joins each acknowledged write to the
actual executing owner/session and receipt, replacing the stale pre-load
forwarding estimate. It does not enforce execution balance or attribute read
owners. The shared-process RustFS test and retained-log replay prove the join;
placement settling and cross-container measurements remain open.

**Loaded scale-out gap at `e50055c48bb`:** the server
[rebalance adapter](../../crab-http-server/src/cells/router.rs) checks every
15 seconds and normally excludes a Cell used within the last 60 seconds.
The [actor](../src/cell/actor/lifecycle/scheduling.rs) refreshes last-used time
for both queries and commands. The [planner](../src/fleet/placement.rs) also
requires two stable observations and 60 seconds from the adapter's first
eligible observation, with at most two transfers per planning batch. Draining
has a separate eligibility path. These rules also exist on the compared main
snapshot; they are not regressions introduced by the recent LTX changes.

At five uniformly scheduled pairs/s across 20 Cells, a Cell is targeted about
every four seconds. Continued traffic therefore prevents its ordinary idle
eligibility. A healthy new container and evenly distributed ingress cannot
establish that old owners shed this workload. This is a source-derived
explanation to test against the retained execution traces, not a causal
attribution of the historical p99. The current qualifier starts each load
after functional checks without a placement convergence gate.

**Design correction:** qualify two explicit scenarios. For settled capacity,
stop application actions while ownership converges, observe signed capacity
and authority without invoking Cell handlers, and require stable ownership
and the declared weighted balance before timing. A fixed 60-second sleep is
insufficient: fresh eligibility evidence, bounded movement and current signed
observations all matter. For scale-out during arrivals, keep traffic running
and measure time to redistribute work. Supporting that case requires bounded
quiescence of a busy Cell through the ordinary drain/publication/release and
takeover gates. Preserve the separate hot-Cell case: moving a single writer
cannot parallelize its workload across nodes.

Existing `fleet_rebalance_donates_ownership_surplus_without_headroom_gain` and
`fleet_rebalance_releases_settled_cell_and_restores_its_result` tests cover
settled transfer. Add continuous-arrival convergence and unaffected-Cell tail
latency to the public service gate; do not reduce production idle guards merely
to obtain an even benchmark chart.

### 13. A streaming decoder still retains avoidable metadata

**Confirmed at audited revision:** [codec::Decoder](../../crab-ltx/src/codec.rs) accumulates the
decoded page/offset/size index and, whenever `replica` is enabled, a second
`EncodedPage` index with frame hashes and page checksums. The latter is built
even when cleanup or ordinary inspection never asks for it. At close, the
decoder reads the remaining stream into a new `Vec`, then allocates another
decoded index to compare with the first. Streaming the input alone does not
remove these allocations. A corrupt early end-of-pages marker can make the
buffered remainder much larger than a valid footer, up to the admitted input
length. This behavior also exists on the main snapshot.

**Change to evaluate:** collect replica index entries only for callers that
need page lookup. Stream and compare footer entries against the observed page
index while checking the exact encoded length and CRC; reject excess bytes
without accumulating the whole remainder. Preserve both supported LTX page
encodings, complete page ordering/coverage, exact digest and trailer checks.
If the remaining observed-page index is material, evaluate admitted file-backed
index scratch separately, including cleanup and cancellation ownership.

**Gate:** measure peak allocations for the same decoded database represented
as compressible and high-entropy cuts, multiple page sizes, and concurrent
verifications under the 1 GiB node profile. Include long invalid tails, early
end markers, truncated varints, and valid CRCs around invalid index entries.
Run the independent Celld/Superfly vectors, bundle and node-frame verification,
exact restore, and sparse publication. A bounded individual `read` does not
prove bounded accumulated decoder memory.

**Implementation:** ordinary verification now collects only the observed
page/offset/size index. Explicit replica inspection collects the frame hashes
and checksums needed for page lookup and moves that index to its caller instead
of cloning it. Footer entries are compared directly with the observed index
through a 64 KiB buffer; neither the complete footer nor a second decoded copy
is retained. CRC and encoded-length checks use the original varint bytes,
including previously accepted nonminimal encodings. Actual I/O failures retain
their source and classification; clean EOF within the footer is corruption.

The long-tail regression fails before this change and passes afterward for
both an early page-end marker and bytes appended after a complete valid file.
Each malformed case reads less than 128 KiB rather than draining its 8 MiB
tail. Independent format vectors retain exact digest, CRC, ordering, coverage,
and restore checks. This bounds excess footer buffering, not total decoder
memory: the observed index still grows with page count, explicit replica
inspection needs additional metadata, and the caller may retain input bytes.
Concurrent peak RSS and same-worker response latency remain open gates.

The [exploratory RustFS comparison](../../crab-ltx/perf/README.md#streaming-footer-and-optional-replica-index-2026-09-26)
observed a 140.5 to 101.9 ms median cleanup for the largest batch across three
processes per implementation. Baseline compilation and unrelated host activity
limit attribution; overlapping whole-process RSS ranges do not prove a memory
improvement. This evidence does not qualify public-action latency or capacity.

### 14. Checksum bookkeeping depends on activation history and uses tiny file I/O

**Confirmed:** fresh `Db::open` and `CellReplica::open_new` start with
`PageChecksums::default()` in [capture initialization](../../crab-ltx/src/capture.rs).
Sparse activation and clean resume seed a file-backed base instead.
[PageChecksums::persist](../../crab-ltx/src/pages.rs) is called after each sealed
cut in [WAL capture](../../crab-ltx/src/capture/wal.rs), before capture returns.
The representations have materially different costs:

| Path | Work at audited revision | Missing qualification |
| --- | --- | --- |
| Fresh Cell, memory base | Allocate a dense array for all database pages and copy the retained base on every cut, even for a small update | Database-size scaling of capture CPU and peak memory with changed pages held fixed |
| Restored/resumed Cell, file base | Read each overwritten old checksum separately; persist each changed checksum with an 8-byte write in hash-map iteration order | Host calls, allocation count, local I/O time, and fragmented versus contiguous changes |
| Clean handoff, file base | `write_dense` buffers output, but obtains each input checksum through a separate 8-byte read | Drain/eviction duration and other Cells waiting on the same worker |

For a 512 MiB database with 4 KiB pages, the memory base is 1 MiB and a dense
file-backed handoff reads 131,072 checksum entries individually. These are
source-derived counts, not measured latency. The default filesystem implements
each positional read with a seek, a new buffer, and a read. The
[resume writer](../../crab-ltx/src/resume.rs) reaches this loop from
`CellExecutor::close_resumable` on the SQL worker. Both representations and
the handoff loop also exist in the compared main snapshot. Existing sparse
capture results therefore cannot establish the cost of a long-lived fresh Cell.

**Change to evaluate:** batch dense sidecar reads as well as writes; use a
bounded checksum-block cache for ordered capture reads and coalesce adjacent
changed checksums before persistence. Measure a shared block-based strategy
for fresh and restored sessions after those changes. Keep transactional
candidate isolation: the old checksum remains available until the cut is
sealed, and partial sidecar failure fences the session. Local scratch is never
recovery authority. The minimal-feature library still needs a tested local path.

**Gate:** fixed-size updates, append, truncate/regrow, sparse activation, clean
resume, and repeated eviction at multiple page sizes. Use `capture_deferred`
in both runtime comparisons: the current replica-cost harness changes capture
durability along with `--sparse`, so its two modes do not isolate this finding.
Record checksum host
reads/writes and bytes separately from SQLite WAL I/O, capture allocation peak,
handoff time, and sibling-Cell p99. Preserve the file-backed overlay tests,
`cell_checksum_write_failure_fences_after_sealing_the_cut`, process-exit
continuation recovery, and same-length local-corruption refusal. Evaluate the
verified whole-file scan in `open_resumed` separately: zero origin reads do not
make warm reactivation constant time, and skipping that scan needs a replacement
integrity proof.

**Implementation:** file-backed persistence now sorts changed pages and writes
contiguous checksums in blocks capped at 64 KiB. Dense handoff reads also use a
64 KiB buffer, consuming base entries even when an overlay replaces them so
later checksums retain their correct offsets. Aggregate verification, post-seal
failure fencing, and the fresh durable handoff sidecar remain unchanged.
Sorting retains one reference pair per changed page; the bounded output buffer
does not make total capture memory constant.

The 32 MiB real-SQLite regression fixture first reproduced 8,210 host reads
for its 65,680-byte handoff sidecar. The same fixture now requires at most two
reads and reproduces identical sidecar bytes. Updating that resumed database
first reproduced 8,757 host writes for its roughly 34 MB cut; the changed path
uses fewer than 1,024, including LTX writes, each at most 64 KiB. It re-reads and
folds the persisted checksum blocks before clean handoff. These operation-count
tests use local files; their unused replica transport is in memory. They do not
measure RustFS latency or isolate checksum writes from all capture writes.

Fresh-memory full-array copies, old-checksum reads during capture, and work on
the shared SQL worker remain open. Multi-buffer overlays, disjoint writes, and
truncate/regrow tests preserve byte equality. Public-action percentiles,
allocation peak, and eviction interference still need qualification.

### 15. Peer verification couples provider latency to scarce CPU admission

**Confirmed at `3cd0bd1bfe6`:** the HTTP
[forward handler](../../crab-http-server/src/peer.rs) reserves a primitive-job
slot before awaiting `NodeDirectory::verify_peer_request`. That method loads
the current signed node advertisement from storage. One slow enrollment read
therefore holds the one-vCPU fixture's only primitive slot, and unrelated
requests receive 503. The same coupling exists in the compared main snapshot.
The node-log handlers load enrollment before acquiring their codec slot;
their larger frame and follower-durability contracts need separate proof.

**Follow-up fix and proof:** the public HTTP/mTLS regression sends five
concurrent comments queries after owner-hint expiry. It reproduced 503s with
both memory storage and RustFS; a temporary probe identified codec admission
refusal. The receiver now decodes the bounded envelope once, releases codec
admission during enrollment I/O, and rechecks the signed enrollment's lifetime
before verifying the request. Request bytes remain reserved while waiting.
The shared ingress hint removes the observed entry catalog/control reads on
the warm path; receiving-owner authority checks remain in place.

Queued codec admission uses the same resource limit as immediate admission.
It explicitly checks the absolute deadline before waiting and after waking:
the pinned Tokio 1.53.1 `Timeout::poll` polls a ready inner future first, so
`timeout_at` alone admitted an already-expired request in the regression.
Nine worker tests now pass, including expired/free-capacity admission, expiration
when a slot opens, cancellation, timeout, shutdown, and resource accounting.
The node-directory test separately rejects an expired enrollment even when its
request signature remains valid. Twelve peer protocol tests preserve strict
payload, unknown-field, duplicate-field, and signature validation.

The HTTP receiver bounds enrollment, resolution, activation, dispatch waiting,
and response encoding by the received transport budget. Initial structural
admission uses the protocol's 60-second ceiling until the envelope's timeout
is available; elapsed admission time still counts against that timeout. The
budget starts after body ingress, and is distinct from the actor's five-second
native-work deadline and the sending client's transport wait. Cancellation of
an HTTP wait does not roll back accepted commands or change unknown-result
resolution.

The TLS regression delays enrollment GETs by one second, proves another codec
job can run during that delay, and checks a 10 ms received budget returns 504.
The public HTTP regression also delays only owner resolution by two seconds
with a 500 ms received budget: it returns 504 without invoking the query
handler, then the same signed query succeeds when the delay is removed.
The full public application regression passes against memory storage and real
RustFS: concurrent hint expiry, owner takeover, restored collaboration state,
and Git clone/tag reads. These are functional proofs, not measured p95 gains.

**Remaining gate:** exercise expiration during activation and accepted-command
completion under HTTP cancellation; retain the runtime stable-identity
cancellation regression.
Measure admission wait, enrollment I/O, codec time, retries, 503s, and retained
request bytes under the one-vCPU profile. Compare public p95/p99 at the same
offered load before accepting the hint and queue changes as a performance win.

### 16. Post-load recovery does not qualify acknowledged tails during load

**Confirmed at `c12b41ef638`:** the scheduled
[load runner](../../crab-http-server/deploy/cell-issue-fleet/load.py) waits for
`drain_publication` before `recover_owner`, which again requires zero uncovered
node-log bytes. It then kills one owner and verifies the latest acknowledged
issue for one selected Cell. Every successful pair has an immediate readback,
but the post-fault check does not revisit all successful request IDs. The
existing README correctly labels this as published-root recovery. The separate
[Compose cluster gate](../../crab-http-server/tests/qualify_compose_cluster.sh)
exercises follower recovery, but it is not the scheduled capacity workload.

**Acknowledgement follow-up:** the load runner now revisits every recorded
acknowledgement before the fault and again after takeover, while the lost node
is still stopped. It verifies exact issue number and title, limits verification
to eight concurrent reads, and records counts by Cell. A real HTTP fixture
keeps the latest issue intact while deleting or changing an earlier result:
the previous recovery function silently accepts both cases, and the follow-up
rejects each with the original request ID. All ten load/provenance tests pass.
This closes the latest-only verification gap; a current-image fleet run and
failure during sustained arrivals remain required.

**Impact:** the current runner cannot establish that low response latency
remains sustainable while publication is delayed, or that every earlier
follower-only acknowledgement survives failure during that backlog. This is
a missing proof, not evidence of lost data. Earlier revisions of the runner
have the same post-drain fault shape.

**Change to evaluate:** add a fault phase while scheduled arrivals continue.
Trigger from observed fleet response proof and a nonzero, position-attributed
unpublished tail, then lose the owner's process and local data. Keep selected
followers available for that case. Run separate follower-loss, delayed-origin,
and ambiguous-publication cases with their declared fault budgets; a combined
fault beyond the durability contract cannot be labeled a supported scenario.

**Missing attribution:** the HTTP issue handler's `command_output` discards
the typed commit receipt. Its submission UUID is a durable application key;
`mutation_identity` creates a different runtime request ID for each attempt.
The runtime response trace includes Cell, sequence, and proof source, while
`cells status` exposes the published root position. Aggregate response and
uncovered-byte counters cannot join those positions to one HTTP submission.
Before using them to trigger a fault, retain a structured submission/receipt
correlation at the application boundary and match it to the owner proof trace.
The trace alone is not an HTTP acknowledgement: the load generator must also
have received and recorded that request's successful response.

**Gate:** retain every acknowledged request ID and expected result, then query
or resolve all of them through public handles after takeover. Count duplicate
effects, unrecoverable results, interrupted arrivals, and recovery delay.
Measure healthy Cells' p99 throughout the fault. The run must show that
publication catches up after origin recovers without discarding accepted work.
Repeat for uniform and hot/skewed workloads at 3, 5, 10, and 20 nodes, including
compatible rollout. Record executing owners during load; a pre-load owner map
cannot identify execution after migration.

### 17. Compaction reads index records individually on the async task

**Confirmed at `9ec6da5176e`:**
[SpoolCursor::advance](../../crab-ltx/src/replica/compaction/source.rs)
calls `FileIo::read_exact_at` for each 60-byte index entry. The default
[filesystem implementation](../../crab-ltx/src/environment/host.rs) performs
a seek, allocation, and read for each call. Remote index downloads already use
bounded chunks; their subsequent local merge does not retain a read buffer.

Both consumers run this iteration outside `Host::run`:
[write_compacted](../../crab-ltx/src/replica/compaction/output.rs) obtains the
next entries before dispatching body decoding and encoding, and
[build_and_upload](../../crab-ltx/src/replica/directory/initial.rs) consumes
the final locator merge while constructing directory nodes. Full compaction
therefore reads the selected index entries and then the new compacted index
again. Truncation can make a merge scan many discarded entries before yielding
one live page. Buffering only the output writer does not address these reads.

**Live diagnostic:** a temporary instrumented run of
`cell_compaction_coalesces_local_output_writes` used the existing 3,000,000-byte
`randomblob` SQLite fixture on a current-thread Tokio runtime. Local RustFS at
port 19010 backed immutable preparation, full compaction, and two restores.
It recorded **1,474 60-byte reads, all on the async thread**, out of 1,480
local reads during compaction. The original and compacted roots restored
byte-identical databases. An in-memory-provider control recorded the same
counts. Production code and existing test assertions were unchanged; the
temporary instrumentation was removed after both runs.

The diagnostic patch and logs are retained outside the checkout under
`$HOME/Workspace/crabbuild-target/crab-8bc8/ltx-index-audit/`:
`rustfs-probe.patch`, `rustfs-probe.log`, `probe.patch`, and `probe.log`.
These are debug operation counts, not latency percentiles, a slow-disk fault
test, or a fleet capacity result. The same per-entry read and async iterator
consumption exist in the compared main snapshot.

**Impact:** source tracing shows that slow scratch reads can hold the Tokio
task between await points, including while the publisher's renewal select is
waiting to regain control. Tokio's
[fairness guarantee](https://docs.rs/tokio/1.53.1/tokio/runtime/index.html#detailed-runtime-behavior)
requires bounded task polling time. The provider concurrency improvements do
not isolate this local work. The existing compaction test bounds output writes;
it does not bound input reads or verify unrelated async task progress.

**Change to evaluate:** retain sequential buffers for index cursors and consume
bounded batches through `Host::run` in both merge passes. Budget the sum of all
cursor buffers, not just one buffer. Bound discarded-entry work as well as live
output pages; preserve merge state across batches. Let the dispatched job own
its file handles, reservations, and scratch until completion. The initial
append's in-memory index iterator shares locator selection but does not need
a file-I/O adapter. Keep newest-wins selection and truncate/regrow handling
canonical in `LocatorMerge`.

**Gate:** count reads by index stream and assert block-scale input I/O; inject
slow and failed scratch reads while checking unrelated Tokio progress and
renewal scheduling. Cover partial-range and full compaction, bundles with
nonzero body offsets, later overwrites, truncate/regrow, cancellation, and
byte-identical restore. Measure retained buffer bytes, scratch peak, foreground
p99, and publication drain through repeated compaction boundaries. This is a
bounded first change before finding 4's larger directory-reuse work.

**Implementation:** both file-backed merge passes now read sequential index
blocks through admitted blocking jobs. All cursors together retain at most
960 KiB of read buffers, with a 60 KiB maximum per cursor. A merge job stops
after 4,096 input entries at the next page-group boundary; a group visits at
most the admitted descriptor count. Discarded entries count toward the budget,
so a long truncated suffix cannot become one unbounded merge job. The shared
newest-wins/truncation resolver remains canonical for in-memory initial
directories and file-backed compaction.

Scratch creation, file opens, reads, writes, syncs, and removal now use host
jobs. Every open scratch file and upload source retains the scratch owner.
Cancellation cannot remove files or release their dirty/recovery/scratch
admission while dispatched work still uses them. Normal completion waits for
cleanup; canceled work schedules cleanup after its last owner drops, using the
same blocking-job ceiling. Cleanup remains best effort on filesystem/executor
failure and requires the Tokio runtime to remain alive.

The existing 3 MB fixture first failed the new read bound with 1,480 local
reads; the async-thread refusal regression also failed before the change.
The updated RustFS diagnostic records **8 local reads** and the same 792
writes, with byte-identical restores of original and compacted roots. This
reduces local read calls by 185 times in that fixture; it does not establish
a latency improvement. The diagnostic delta and log are
`rustfs-after-test.patch` and `rustfs-after.log` in the artifact directory above.

Seven focused compaction tests pass, including six canceled filesystem stages
with paused cleanup and one job slot. All 26 exact-root/sparse cases pass;
the truncate/regrow fixture now spans multiple merge jobs and verifies both
partial and full compaction. Replica all-target Clippy passes with warnings
denied. Whole-graph metadata work, the conservative scratch admission floor,
and sustained foreground/publication measurements remain open.

### 18. Cold directory-cache fills retain origin admission and delay readers

**Confirmed at `7d3dd9232b0`:**
[read_node](../../crab-ltx/src/replica/directory.rs) takes `Host::io_permit`,
downloads and authenticates a directory object, then awaits
`directory_cache_put` before inserting the bytes into memory and returning.
The permit remains in scope during that await.
[Host::directory_cache_put](../../crab-ltx/src/environment/host.rs) dispatches
blocking cache work under a separate job permit.
[DirectoryCache::put](../../crab-ltx/src/environment/directory_cache.rs)
syncs the new file and renames it, then clones, serializes, syncs and renames
the membership index. The filesystem contract also requires durable parent
installation. A slow cache device can therefore delay an already verified
read and occupy origin capacity needed by unrelated requests.

This affects cold/missed directory reads used by activation, sparse faults
and publication metadata. Memory hits and verified disk-cache hits bypass the
fill; the earlier removal of hit index writes does not close this path.
The same fill ordering exists in the compared main snapshot.

**Diagnostic:** a temporary public-API host-hook probe captured a real SQLite
row and prepared an immutable root, then reopened it through a distinct store
identity with a cold directory cache and one I/O permit. Pausing cache
`sync_all` left the root read unfinished and the permit count at zero; another
permit waiter timed out after 100 ms. Releasing the filesystem pause returned
the root and restored the permit. The probe reproduced with an in-memory
control and with local RustFS at port 19010. The RustFS root then restored
into a fresh SQLite file and `SELECT count(*) FROM t` returned the captured
row. The 100 ms wait is an injected diagnostic bound, not a service percentile.

The production source and existing assertions were unchanged. Temporary
instrumentation was removed after the run. Patches and logs are retained under
`$HOME/Workspace/crabbuild-target/crab-8bc8/ltx-design-audit/` as
`cache-admission-memory.patch`, `cache-admission.log`,
`cache-admission-rustfs.patch`, and `cache-admission-rustfs.log`.
The eight existing `cell::roots::directory` tests also pass on the restored
source, including corruption, restart, incremental update and truncate/regrow.

**Best next fix to evaluate:** first end origin admission once transfer and
bounded authentication finish. This isolates network capacity while retaining
the current cache contract. Then evaluate returning authenticated bytes before
optional cache installation through a bounded, deduplicated fill queue owned
by the host. Account queued bytes, dispatched jobs and disk reservations;
shutdown must drain or cancel undispatched work and await dispatched work.
Do not replace the await with unlimited detached tasks. Tokio's
[blocking-task contract](https://docs.rs/tokio/1.53.1/tokio/task/fn.spawn_blocking.html)
requires dispatched work to retain its resources until completion.

**Gate:** preserve digest, symlink, restart, eviction and disk-budget proof.
Pause/fail cache writes and index syncs while a second Cell reads or publishes;
the second origin request must progress after the first transfer completes.
Exercise canceled readers and concurrent fills for one key. Measure first
read/first mutation, cache fill queue age, blocking-job wait and foreground
p99 with empty and churned caches under the 1-vCPU/1-GiB profile. Network
permit isolation alone does not establish foreground latency isolation from
the shared blocking executor or cache-index rewrite cost.

**Implementation follow-up:** origin admission now ends after the bounded
download and digest check. Cache fills take immediate blocking-job admission;
when the pool is busy they skip optional persistence instead of retaining
verified buffers in an unbounded wait queue. Admitted fills share the same
dispatch/ledger/cancellation owner as other host jobs. Cache-hit reads,
invalidation, digest checks, and persistent installation are unchanged. Skips
may increase origin reads after a restart; cache contents remain derived data.

Both new regressions failed before the change. With a cache fsync paused, a
separate cold origin read now completes using the same one-permit I/O pool;
aborting the first reader retains its blocking slot until the fsync finishes.
The second root restores into a fresh SQLite file with the captured row
visible. The same test passes against real RustFS and is retained as an
explicitly invoked host test, documented in the
[LTX verification runbook](../../crab-ltx/README.md#verification). A private
admission regression proves that busy job slots do not queue fill buffers and
that a subsequent admitted fill persists normally.

The reader performing an admitted fill still waits for local persistence.
Disk-cache lookups also use the shared blocking pool. Host-owned write-behind,
foreground isolation, cache-fill hit-rate effects and public action latency
remain open; this change does not claim those gates.

### 19. The initial asynchronous draft still blocked its own Cell and treated every error as stale

**Confirmed in committed coordination and the hydration draft:**
[`BeginHydration`](../src/coordination.rs) sets `busy = true`, and `Schedule`
will not start queued work until `FinishHydration` clears it. The
[background task](../src/cell/actor/lifecycle/background.rs) holds this state
across preparation, asynchronous fetch and installation. Releasing the SQL
worker therefore helps sibling Cells, while a new request to the hydrating
Cell still waits for remote I/O, even when its required pages are resident.
An empty queue when hydration begins does not bound the latency of later
arrivals. This coordination behavior also exists in the compared main snapshot.

The same task fences on every returned error, and
[`handle_hydrated`](../src/cell/actor/tasks/activation.rs) maps every error to
`stale: true`. The draft defers retained-byte admission pressure, but a remote
timeout before installation still follows the fencing path. The previous
synchronous VFS path could have partially installed pages before failing;
the new fetch stage has a stronger no-local-mutation boundary. Its error policy
has not yet taken advantage of that distinction.

**Change to evaluate:** track in-flight hydration separately from exclusive
foreground work in the pure coordination state. Keep the effect alive for
drain, shutdown and ownership checks; reserve the worker only for preparation
and installation. Prefer foreground work between bounded installation batches.
Classify a retryable pre-install fetch failure as deferred maintenance only
while the owner remains valid and no local installation occurred. Retain
fencing for lease loss, integrity failure, uncertain partial installation and
stale activation. Simply clearing `busy` without changing completion handling
can let a hydration completion clear a concurrent command's state; it is not
a sufficient fix.

**Gate:** use a public Cell handle to read a resident row and perform a mutation
while a different inherited range is stalled. Prove queue progress, the exact
captured successor root, and no stale overwrite after checkpoint/truncate.
Repeat for owner loss, drain, canceled fetch, transient transport failure,
corruption and installation failure. Existing
[`residency` tests](../tests/runtime/lifecycle/residency.rs) cover shutdown,
origin failure, disk exhaustion, lease loss and takeover; the worker-only
probe does not exercise this actor boundary. Preserve those failure guarantees
while adding the distinct safe-to-defer outcome.

**Implementation:** hydration now owns a separate effect without retaining
the foreground `busy` slot. Completion updates only residency, preserving any
concurrent command's busy state. Pending effects still prevent early drain or
transfer. Preparation/fetch timeouts, retryable fetch failures and pre-install
capacity refusal return a deferred outcome; the actor waits at least one second
and honors longer provider delays. An installation timeout remains ambiguous
and fences the owner, as do permanent failures. The worker owns these phase
deadlines; the actor no longer wraps all phases in an indistinguishable timeout.

The public-handle regression first failed its one-second foreground deadline.
It now queries and mutates a resident row while a different hydration GET is
paused, then restores the exact published root and reads the mutation. A
second public test exposes one transient provider failure with storage retries
disabled, proves the owner remains serving, and waits for hydration to resume.
Seven lifecycle tests retain shutdown, permanent-origin failure, disk exhaustion,
lease-loss and takeover coverage. Forty-six coordination/simulator cases pass,
including hydration completion while a command owns the foreground slot.
Four worker tests also cover fetch cancellation, deadline deferral, retained-byte
pressure and progress on both workers. The public HTTP/mTLS RustFS collaboration
test passes with the split enabled: a forwarded application mutation is
acknowledged, publishes LTX and remains visible through the application query.
This is end-to-end correctness evidence, not a service performance qualification.

### 20. Sparse activation serializes unrelated Cells through a global registry lock

**Confirmed at `3611a7895f6`:** [`Registration::new`](../../crab-ltx/src/writable_vfs.rs) locks
the process-wide `views()` map before creating and sizing the sparse file,
syncing it and its parent, constructing the paged I/O bridge, reserving local
disk, and allocating the presence map. It releases the lock only after
inserting the activation. `x_open` and registration teardown use the same map.
Consequently a slow local sync can delay another sparse activation or its
teardown on a different SQL worker. Already open SQLite reads do not acquire
this registry lock; the risk concerns activation and lifecycle concurrency.

The entry path is `CellWritableDatabase::open_writable` from
[`ActivateRestored`](../src/cell/worker/run.rs); eager checksum preparation is
earlier and asynchronous. Moving checksum I/O to `Host::run` therefore does
not fix this separate critical section. The compared main snapshot has the
same lock scope. Its fleet latency contribution is unmeasured.

**Change to evaluate:** keep path claiming and registration publication atomic
under short map operations; perform filesystem barriers, bridge startup and
allocations outside the global lock. Preserve exclusive fresh-destination
ownership and remove only the failed activation's own claim. Ensure teardown
drops resource owners after releasing the map lock, including a last bridge
reference whose destruction joins its I/O worker. Avoid one thread or one
registry per Cell.

**Gate:** pause activation A's `sync_all` or `sync_parent`; activation B on a
distinct path and teardown C must finish independently. Attempt the same path
twice and require one winner; inject each setup failure and prove no stranded
claim or removal of another activation. Existing
[`activation` hook tests](../../crab-ltx/tests/host/hooks/activation.rs) test
checksum preparation and cancellation, but do not prove this global-lock
isolation. A recovery storm with constrained disk IOPS is the service test.

**Implementation:** the registry now reserves a canonical path under a short
lock, represented by a private claim guard and an initially empty weak
reference. The guard alone can publish or remove that entry. File creation,
size, file/parent barriers, bridge startup, disk admission and presence-map
allocation all run after releasing the lock. A completed activation publishes
its weak discovery reference; `xOpen` upgrades it while holding the map lock,
then retains the same strong file ownership as before. The registry cannot
destroy an activation or join its bridge while locked. This uses Rust's
[weak-reference ownership contract](https://doc.rust-lang.org/std/sync/struct.Weak.html).

A failed setup drops its claim. Existing or partially created files remain
quarantined under the exclusive-create contract; cleanup does not remove local
files or another activation's claim. Canonical path resolution and WAL-sidecar
refusal are unchanged, as are SQLite file callbacks and one-writer authority.
The change adds 14 net production lines and no public API or configuration.

The first public-API regression reproduced both unrelated open and close
missing their one-second bound while a file sync was paused. The strengthened
test pauses file sync, parent sync and bridge startup separately, and checks
distinct selected-root values for all three Cells. See the
[RustFS run command](../../crab-ltx/README.md#verification) for the same scenario
with real objects. Failure cases cover file creation, sizing, both barriers,
bridge startup, capture-directory creation and a pre-existing destination.
Repeated conflicting opens must refuse promptly without releasing the first
caller's claim. Per-activation disk/bridge waits still occupy its assigned SQL
worker; this fix does not qualify aggregate cold-start latency or disk capacity.

The seven activation tests pass, along with ten sparse LTX tests, eight
minimal-feature integration tests and all 24 runtime residency tests.
Both the new isolation test and the public HTTP/mTLS
application/takeover test pass against local RustFS. Replica all-target Clippy
passes with warnings denied. These checks cover registration, failure cleanup,
hydration and restored application visibility; they do not establish a fleet
latency percentile. Before/after and RustFS logs use the `activation-registry-`
prefix in this checkout's external target directory.

### 21. The initial asynchronous draft lost demand-read cache reuse

**Confirmed in the draft, absent as a separate path on main:**
[`Io::page`](../../crab-ltx/src/paged_io.rs) reads the shared view-keyed page
cache and fetches ahead on a miss. `Io::hydration_pages` instead calls the
database's `read_run` directly, without consulting or filling that cache.
The VFS presence map records installed pages, not all prefetched pages.
Hydration can consequently download and decode pages already fetched by an
earlier SQL fault. The two-versus-three operation diagnostic in finding 9 is
consistent with this path difference; it does not isolate bytes, cache hits,
or a general throughput regression.

Both paths still authenticate through the same replica reader. That reader
also computes a window's spans and consumes its first span (finding 8).
Fragmented roots make the draft fetch several spans serially. Sixty-four
pages bound payload, not network round trips or wall time; enough slow spans
can consume the five-second hydration deadline and trigger finding 19.

**Change to evaluate:** share authenticated range/cache access between demand
reads and background fetch without reacquiring a synchronous SQL-worker wait.
Use exact view/root identity, bounded cache ownership and single-flight work
where useful. Evaluate bounded multi-span fetching for bulk hydration and
smaller/adaptive demand read-ahead separately. Account for encoded frames,
decoded payload and queued installation bytes together; the draft's retained
reservation covers page payload, not every transient allocation.

**Gate:** reuse one fixed immutable root for before/after runs. Warm a known
range through SQL, then hydrate it under cold, warm and churned caches.
Measure useful prefetched pages, duplicate range bytes, fetch waves, decode
CPU, peak retained/RSS bytes, hydration completion and sibling/own-Cell p99.
Repeat with alternating-object page locators, cancellation and a full retained
budget. Preserve no cursor advancement on an abandoned batch and exact-root
verification; never treat cache content as recovery authority.

**Implementation:** the asynchronous fetch now consults the same view-keyed
demand cache. It returns a cached prefix directly and stops a missing fetch
before a cached suffix. Fetched pages destined for immediate installation do
not create another cache. The regression warms one immutable activation through
SQLite opening, then installs eight missing pages: the initial draft made two
range calls; the changed path makes zero. Ten sparse LTX tests pass, including
checkpoint supersession, truncate/regrow, foreign activation and exact restore.
Concurrent demand/fetch single-flight, fragmented multi-span fetching and total
memory accounting remain separate qualification work.

### 22. Reopening a persistent directory cache blocks async activation

**Confirmed at `e50055c48bb`:** the async runtime acquisition paths call
[`replica_with_directory_cache`](../src/cell/actor/acquire.rs), which calls
[`Host::with_directory_cache`](../../crab-ltx/src/environment/host.rs)
synchronously. [`DirectoryCache::with_budget`](../../crab-ltx/src/environment/directory_cache.rs)
cleans temporaries, reads/parses the complete index, checks each file's length
and canonical path, and acquires its disk reservation before returning.
This constructor bypasses the admitted executor used by subsequent cache
reads and fills. Slow cache storage can occupy a Tokio worker even though
checksum-file creation and sparse registry setup have been isolated elsewhere.
Tokio's [fairness contract](https://docs.rs/tokio/1.53.1/tokio/runtime/index.html)
requires bounded task polling; wrapping this constructor in an async function
does not move its filesystem calls off that worker.

There is a second, independent cost: for each accepted entry, construction
sums **all previously retained lengths** to check the byte cap. With `n`
valid entries below the cap this visits `n(n-1)/2` lengths; at the 16,384-entry
cap, 134,209,536 length visits occur before the final total. The constructor
also validates entries before applying the count cap. This is source-proven
bookkeeping complexity, not a measured fraction of HTTP response time.

**Reproduction:** the ignored
[`directory_cache_restart_diagnostic`](../../crab-ltx/tests/host/hooks/activation.rs)
seeds a version-1 membership index and 1 KiB local files, then reopens it three
times with a 256 MiB disk budget and a derived 32 MiB cache cap. It verifies
membership, retained bytes and reservation release. Production source remains
`e50055c48bb`; only this diagnostic was added. On macOS arm64, mounted APFS,
Rust 1.97.0 and an optimized build:

| Entries | Constructor samples (ms) | Median (ms) |
| --- | --- | ---: |
| 1,024 | 34.562 / 33.788 / 34.170 | 34.170 |
| 4,096 | 142.539 / 153.516 / 135.378 | 142.539 |
| 16,384 | 757.404 / 762.246 / 713.062 | 757.404 |

These measurements include filesystem metadata and bookkeeping. They do not
isolate the fold, inject disk delay, exercise RustFS, measure authenticated
directory lookup, or establish a service percentile. Files were seeded locally
before timing; OS caches were not flushed. The debug run also passes and is
retained separately. Raw output is `audit-cache-restart-e500-release.log` and
`audit-cache-restart-e500.log` beneath this checkout's external Cargo target.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
TMPDIR="$HOME/Workspace/crabbuild-target/crab-8bc8/tmp" \
  cargo test -p crab-ltx --features replica --test host \
  directory_cache_restart_diagnostic --release --locked -- --ignored --nocapture
```

**Change evaluated:** maintain a checked running byte total during
reconstruction, and have runtime acquisition await one admitted blocking cache
construction job. Bound index input before allocation and avoid
validating an arbitrary number of entries beyond the accepted cache envelope.
Preserve canonical-path checks, private-temporary cleanup, reservation ownership
and origin verification. Cancellation must retain the dispatched job and its
reservations until it finishes. A cache remains an optional accelerator.

**Gate:** repeat the entry-count curve with unchanged fixtures and slow metadata
I/O; prove an unrelated timer and resident action progress while acquisition
waits. Test canceled acquisition and simultaneous restarts against one shared
disk budget. Existing cache restart/eviction, corruption/symlink, concurrent-fill
and exact-root cache reuse tests protect behavior but do not cover constructor
latency or executor isolation. Full replay, cold acquisition and warm reacquisition
callers must use the same construction seam. The audited constructor and runtime
call also exist on `origin/main` snapshot `de0bb234abc`.

**Implementation:** reconstruction now retains a running byte total, checks
the 16 MiB index-input cap before reading and checks membership/byte admission
before validating each file. `Host::with_directory_cache` now awaits the existing
admitted executor and returns a `Result`. All workspace callers await that one
path. This builder is absent from release tag `v1.2.4`; no synchronous alias or
new root export was added. The separate local capture APIs remain synchronous.

Tracing the caller found repeated cache construction within one activation:
the final publisher path passed an already selected directory into a helper
that took its parent again. The public runtime regression reproduced five
cache openings across bootstrap, cold acquisition and clean reacquisition,
including incorrect parent directories. Each acquisition entry now creates
one cache host before recovery, and passes its clones through SQLite and
publication. The internal activation stages no longer replace that host.

The off-async-worker and oversized-input regressions both fail against
`e50055c48bb` and pass with the change. A current-thread Tokio test pauses after
the first disk reservation, cancels its waiter and proves the blocking slot and
bytes remain held until dispatched work finishes. Concurrent cache construction
also shares a deliberately constrained disk budget without overcommitting it.

With only the running-total change applied, the optimized diagnostic's
16,384-entry median fell from 757.404 ms to 528.169 ms. Filesystem validation
remains material; moving it off the async caller is a separate improvement.
This is one before/after diagnostic series, not a service percentile or a
supported recovery limit. Raw output is
`cache-construction-running-total-release.log` beneath the external target;
the failing seam logs are `cache-construction-before.log` and
`cache-owner-before.log`.

The final optimized async-construction probe reports medians of 31.406,
130.442 and 524.585 ms for the same three entry counts; retain
`cache-construction-final-release.log` separately from the single-change
experiment. Six cache unit tests, eight directory cases, eleven activation
cases, twenty-five runtime residency cases and eight minimal-feature LTX cases
pass. The real RustFS HTTP/mTLS collaboration/takeover test and public CellNode
primitive takeover test pass, including the acknowledgement trace join in the
HTTP case. Replica/runtime all-target Clippy, formatting and documentation
validation pass. These checks do not qualify fleet recovery percentiles.

### Re-audit decision and proof gaps

**Is this the best fix for the reproduced waits?** Splitting remote fetch from
owner installation and separating its effect from foreground ownership remove
the worker and actor waits at their respective boundaries. Reusing the demand
cache removes the demonstrated duplicate reads. Short registry claims also
remove disk/setup waits from unrelated activation and teardown. Installation
and individual activation still occupy their SQL worker.
Demand SQLite VFS callbacks still return
synchronously under the [SQLite I/O contract](https://www.sqlite.org/c3ref/io_methods.html).

The highest remaining sustained-write opportunity is still range-proportional
compaction and publication drain (4–5), followed by bounded root coalescing if
measured debt justifies it. The highest recovery-size opportunity is bounded
authenticated checksum blocks (3, 14). These changes preserve one fenced writer,
one ordered root publisher and each command's stable receipt. Raising queues,
worker counts or provider concurrency alone does not remove the underlying work.

The public application qualification must exercise Entity, Shard, Workflow and
read models through CellNode/application handles. Require fixed offered load,
actual execution distribution, local/forwarded and read/write action traces,
published-versus-acknowledged rates, and faults during an unpublished acknowledged
tail at 3/5/10/20 nodes. One shared Compose host cannot qualify independent-host
failure or aggregate dedicated CPU capacity.

The staged reference-suite relocation still fails its old suite inventory
entry and remains outside this implementation. Root rules require approval for
that inventory change. Hydration's new public types live under `crab_ltx::db`,
beside their owning `Db` API; the frozen root prelude is unchanged. No inventory
was edited to suppress a failure. Full-plan completion remains unproven.

## Safety and proof retained by the audit

Both the [ARM64 image/Compose run](https://github.com/crabbuild/crab/actions/runs/36239430827)
and [AMD64 image/Compose run](https://github.com/crabbuild/crab/actions/runs/36239424906)
passed at `e50055c48bb`. The ARM64 image was checksum/source verified and
imported for the next fixed-workload fleet comparison. Those CI receipts cover
the preceding hydration and registry changes; they exclude the cache-construction
change above and do not establish sustained service capacity.

The hydration follow-up passes 67 focused worker, coordination, sparse LTX and
public lifecycle tests, plus all eight minimal-feature LTX integration tests.
Runtime and replica-enabled LTX all-target Clippy pass with warnings denied;
workspace formatting and runtime documentation validation pass. The real RustFS
worker diagnostic and public HTTP/mTLS application test also pass. These checks
cover the implementation recorded in findings 9, 19 and 21; they do not close
the installation-latency, recovery-storm or sustained fleet gates.

The action-tracing source `2bf1967c7f3` subsequently passed both the
[ARM64 image and Compose run](https://github.com/crabbuild/crab/actions/runs/36235888087)
and the [AMD64 image and Compose run](https://github.com/crabbuild/crab/actions/runs/36235876451).
Those receipts predate the hydration follow-up and must not be attributed to it.

The action-tracing change passes the real RustFS HTTP/mTLS application test and
replays its formatter output through the same join CLI used by the fleet
runner. Twenty-three fleet tests cover scheduled arrivals, HTTP retry identity,
provenance and attribution refusal. Thirteen runtime durability cases and two
command/effect cancellation cases pass with the instrumentation. Runtime and
HTTP all-target Clippy pass with warnings denied; actionlint and runtime docs
validation pass. These checks do not establish tracing overhead or fleet
performance. The unrelated staged reference-suite relocation still fails the
layout inventory gate pending its previously requested approval; it is not
included in this tracing change.

The placement-parity failure at `c6870fd6a88` is reproduced as a sampling race:
metrics returned one active Cell while its signed advertisement still returned
zero. A live RustFS probe on the unchanged `c12b41ef638` publisher observed
the same process converge to one in a newer advertisement about 1.2 seconds
later. The publisher/count source is unchanged between those revisions.
The original one-shot assertion rejects the retained mismatch consistently.

The Compose collector now brackets each advertisement with fresh metrics,
checks static capacities and the expected live session, and requires the same
active count at two increasing generations on one unchanged container boot.
It fails on persistent mismatch, stagnant/regressing advertisements, malformed
metrics, restarts or the 30-second deadline. The final receipt retains the
matched node/metrics; the run log retains the convergence trace. Thirteen
collector/recovery tests pass, and the live collector passed on the RustFS
fleet with generations 1037/1038 in 2.65 seconds. The raw result and trace are
`placement-collector-live.json` and `placement-collector-live.log` beneath
the checkout's external target directory. This proves the collector against
that running image. The subsequent
[ARM64 image and Compose run 36233256995](https://github.com/crabbuild/crab/actions/runs/36233256995)
passed at `5feef9968e6`, including the collector and prior all-acknowledgement
changes. The [image workflow 36234166704](https://github.com/crabbuild/crab/actions/runs/36234166704)
also passed at cache-admission revision `efcef4b1ffa`. Neither run covers the
later action-tracing changes or sustained 3/5/10/20-node load curves.
The earlier shutdown refusal about an unsealed node log is a separate open
lifecycle observation.

The workflow linter also reproduced eight `SC2016` failures in the existing
candidate-reuse source checks on the main snapshot. Their literal patterns now
use a quoted here-document and one mandatory check per line. Pinned actionlint
v1.7.11 passes both affected workflows, the source checks pass, and removing
each of the eight required fragments independently makes the check fail.
No warning baseline or qualification assertion changed.

The follow-up [ARM64 image and Compose run 36227844137](https://github.com/crabbuild/crab/actions/runs/36227844137)
and [runtime/LTX property run 36227842782](https://github.com/crabbuild/crab/actions/runs/36227842782)
both pass at `c12b41ef638`. The image run retains qualification evidence and
the exact-source Linux image. This closes those two pending CI runs; it does
not supply sustained 3/5/10/20-node curves or a diagnosis of the earlier
placement assertion failure. The all-acknowledgement generator follow-up at
`9ec6da5176e` and the staged reference-suite relocation are outside that source
receipt. They still need their respective end-to-end and policy gates.

The follow-up audit's HTTP/mTLS test passes with both in-memory
storage and real RustFS, including five concurrent calls after hint expiry,
owner loss, restored collaboration state, and Git clone/tag reads. Both tests
completed in 13.05 seconds together; that duration is not an action-latency
sample. Focused admission, enrollment-expiry, and signature regressions pass.
The runtime accepted-command cancellation test also passes. All-target Clippy
with warnings denied passes for LTX, runtime, and HTTP, including the minimal
LTX feature set. Replica-only continuation helpers now share their callers'
feature gates. The decoder fixture helper lives in the existing test module;
no test allow-list or warning baseline changed. Activation-delay injection,
performance measurement, and broad CI remain open as described in finding 15.

Committed-source ARM64
[run 36224838843](https://github.com/crabbuild/crab/actions/runs/36224838843)
built the `3cd0bd1bfe6` image but failed the Compose qualification at
`assert_placement_parity` in `qualify_compose_cluster.sh:448`. The assertion
compares a live advertisement's placement values with capacity and metrics
observations. The log does not isolate which conjunct failed; do not classify
this as an LTX latency regression, data loss, or a passing image qualification.
Retain the failure and diagnose the snapshots before another capacity claim.

The checksum batching follow-up passed 37 focused cases: two file-overlay
cases, eleven capture/failure tests, seven modeled-crash cases, five resume
integrity cases, process-exit continuation recovery, and eleven independent
format/restore cases. Replica all-target Clippy with warnings denied and the
minimal-feature local roundtrip example pass; the latter restored its visible
issue after deleting the source database. The same three minimal-feature
unused capture/checksum warnings remain. The combined routing/checksum tree
also passed the real RustFS HTTP/mTLS application-mutation, owner-takeover,
restored-collaboration, and Git-read test. These prove functional behavior;
no checksum latency SLO or fleet capacity is claimed.

The follow-up audit reran the LTX prune-accounting fault test and the runtime
published-root/local-prune-failure test; both passed. The scheduled runner's
six HTTP/scheduler tests also pass, including stopping when an acknowledged
issue returns 404. Local RustFS uniform and overload smoke results are
[recorded with their image limitation](../../crab-http-server/deploy/cell-issue-fleet/README.md#scheduled-harness-smoke-2026-09-26).
Those results prove the harness and published-root recovery, not a latency SLO.

Streaming cleanup passed 44 host-hook tests, six bundle cases, two node-frame
cases, ten independent format/restore cases, four minimal-feature codec cases,
and the runtime published-root/local-prune-failure test. The LTX replica build
passes all-target Clippy with warnings denied. At that revision the minimal-feature tests emitted unused capture/checksum
warnings; no warning baseline or policy inventory changed. The cost runner built in release mode and completed
the real RustFS comparison above. Remaining decoder memory and fleet latency
gates are explicitly open.

The decoder follow-up passed 62 focused cases: seven replica codec, eleven
independent format/restore, 26 sparse/exact-root, two external-vector replay,
six minimal-feature codec, six bundle, two node-frame, and two public cleanup
cases. Replica all-target Clippy with warnings denied and the release cost
runner build passed; that revision retained the same minimal-feature warnings. Deep decoder
fuzzing and the broader runtime paths are delegated to the existing CI gates.

Current-source ARM64
[CI run 36216278190](https://github.com/crabbuild/crab/actions/runs/36216278190)
built the image and receipt validator but failed the cluster gate waiting for
the elected successor's recovery-work counter after owner loss. The script had
already verified recovered issue visibility. The missing counter evidence must
not be treated as data loss or successful image qualification. Source tracing
found that the gate sampled only the elected successor, which need not be the
recovery claimant, and compared it with physical `server-c`'s earlier counters.
The qualifier now derives work from each surviving process's own before/after
snapshots, rejects resets or missing data, and retains those snapshots in the
receipt. Six deterministic cases cover the distinct claimant/owner roles,
aggregation, process changes, invalid counters, and inactive-log zero work.
The native ARM64 [follow-up run 36219430132](https://github.com/crabbuild/crab/actions/runs/36219430132)
passed the corrected Compose gate at `a16c8efcc2b`, including owner loss and
follower recovery. That result qualifies that source's functional fault path;
it predates decoder commit `0360485311b` and the routing/checksum changes above.
It supplies neither sustained 3/5/10/20-node latency curves nor current-head
image qualification.
The [HTTP container run at `0360485311b`](https://github.com/crabbuild/crab/actions/runs/36220235482)
also passed, along with that revision's decoder fuzz workflow. Routing commit
`4e0d71fe43d` and the checksum batching above still need their own image proof.

The no-job property-workflow failure has a reproduced configuration cause:
job-level `env` referenced `runner.temp`, but GitHub only exposes that context
at the later [step environment boundary](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability).
The repository's pinned actionlint v1.7.11 rejects the previous file at that
expression. Moving the existing target-directory setting to both Cargo steps
passes the same check and preserves the 1,000-case workload. The repaired
[property run 36222852526](https://github.com/crabbuild/crab/actions/runs/36222852526)
passed both runtime and LTX suites at `8586757a6eb`. The follow-up
[property run 36224241056](https://github.com/crabbuild/crab/actions/runs/36224241056)
passes at worker-admission revision `b8798fdcfdc`.
[Native ARM64 container run 36222681957](https://github.com/crabbuild/crab/actions/runs/36222681957)
passes at `7b20ebe484f`; it predates worker admission and image-provenance
enforcement. The separate app-to-host dev-dependency policy failure remains open.

Seven existing tests passed locally with real SQLite and in-memory object
storage: four `environment::tests::directory_cache` cases, missing cached-root
metadata refusal, scheduled compaction with byte-identical restore, and
contiguous sparse hydration coalescing. These validate retained invariants;
they do not measure the proposed optimizations or RustFS latency.

Run the same focused checks with a checkout-specific external target:

```sh
export CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8"
export TMPDIR="$CARGO_TARGET_DIR/tmp"
test -d "$TMPDIR" && test -w "$TMPDIR"
cargo test -p crab-ltx --features replica --locked --lib environment::tests::directory_cache
cargo test -p crab-ltx --features replica --locked --test cell warm_root_cache_does_not_mask_missing_metadata
cargo test -p crab-ltx --features replica --locked --test cell scheduled_cell_compaction_promotes_fanout_and_preserves_root
cargo test -p crab-ltx --features replica --locked --test cell sparse_hydration_coalesces_contiguous_cell_frames
node crates/crab-cell-runtime/docs/validate.mjs
```

Keep `synchronous=FULL` while measuring these changes. SQLite documents that
`NORMAL` in WAL mode can lose committed transactions after power loss;
[the upstream contract](https://www.sqlite.org/pragma.html#pragma_synchronous)
requires any relaxation to be justified by the external-proof and fresh-restore
model, including crash tests. Removing redundant cache-index syncs has a much
narrower proof obligation than changing SQLite commit durability.
