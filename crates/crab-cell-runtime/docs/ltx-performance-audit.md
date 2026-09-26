# Audit LTX latency and sustained publication capacity

| Document intent | Value |
| --- | --- |
| Content type | Design audit and acceptance gates |
| Audience | LTX, runtime, storage, and qualification contributors |
| Scope | Initial baseline `0f3f4f7617a`; committed follow-up through `3cd0bd1bfe6`, plus the routing/admission follow-up below; compared with `origin/main` snapshot `de0bb234abc`. |
| Status | Small uploads, cache hits, streaming cleanup, decoder metadata, checksum I/O batching, and cross-worker admission improved; same-worker isolation, publication capacity, public latency, and fault-under-load qualification remain open |

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
| P1 | Peer admission can turn provider delay into rejection or excessive waiting (15) | Concurrent hint expiry with delayed enrollment reads; bound request memory and the complete pre-dispatch wait |
| P1 | Cross-Cell SQL worker blocking (9) | Slow one sparse Cell while reading a resident Cell on the same worker; repeat on different workers |
| P1 | Whole-graph compaction and serial publication debt (4–5) | Sustained updates through repeated debt thresholds; compare response rate with publication rate |
| P1 | Cleanup on the SQL worker and decoder memory (10, 13) | Body/footer buffering and unused replica indexes removed; measure remaining index, confirmation time, RSS, and sibling-Cell latency |
| P1 | Execution load is not proven balanced (12) | Record owner/execution distribution and fixed offered load at every scale stage |
| P1 | Capacity runs do not fault outstanding follower-only acknowledgements (16) | Kill an owner during sustained arrivals with a proven unpublished tail; verify every acknowledged request after takeover |
| P2 | Eager checksum metadata and demand read amplification (3, 8) | First query **and first mutation**, point/random/scan workloads, cold and churned caches |
| P2 | Checksum maintenance differs between fresh and restored Cells (14) | Hold database and changed-page count fixed; compare capture, clean handoff, host I/O calls, and allocations in both states |
| P2 | Checkpoint tail cost and shared maintenance resources (6, 11) | Long update runs with checkpoint, hydration, and compaction interference |

These priorities identify code-supported risks and missing evidence. They do
not rank measured contributions to public p99: the required action-level
phase measurements are still missing. The best next fix should remove work
from a measured critical path while retaining the existing authority and
durability contracts. Raising concurrency or queue capacity alone does not
meet that criterion.

## Architecture decision after this audit

Keep one SQLite writer per Cell and immutable, verified LTX roots behind the
authority CAS. The best next change is the smallest ownership change that
removes a measured wait or repeated work while preserving those contracts.
This is not yet a verdict that the current PR meets the performance plan.

1. Complete peer admission and same-worker interference qualification first.
   A low-latency local capture cannot compensate for an ingress rejection or
   a worker waiting on another Cell's storage request.
2. Make range compaction proportional to the affected metadata before adding
   publication concurrency. Concurrent work must never create competing root
   publishers for one Cell. If publication still cannot drain, evaluate a
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

**Change to evaluate:** reuse authenticated unchanged directory branches and
update locators only where selected segments still supply the current page.
Measure before making compaction concurrent with publication: any such change
needs an exact predecessor check and must discard or safely rebase stale work.

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
origin reads also retain their I/O permit while awaiting disk-cache insertion.

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
The same-worker origin wait, foreground-aware maintenance, asynchronous
hydration, and proof-to-confirmation latency gates remain open.

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

**Confirmed:** [run_stage](../../crab-http-server/deploy/cell-issue-fleet/qualify.py)
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

**Confirmed:** the scheduled
[load runner](../../crab-http-server/deploy/cell-issue-fleet/load.py) waits for
`drain_publication` before `recover_owner`, which again requires zero uncovered
node-log bytes. It then kills one owner and verifies the latest acknowledged
issue for one selected Cell. Every successful pair has an immediate readback,
but the post-fault check does not revisit all successful request IDs. The
existing README correctly labels this as published-root recovery. The separate
[Compose cluster gate](../../crab-http-server/tests/qualify_compose_cluster.sh)
exercises follower recovery, but it is not the scheduled capacity workload.

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

**Gate:** retain every acknowledged request ID and expected result, then query
or resolve all of them through public handles after takeover. Count duplicate
effects, unrecoverable results, interrupted arrivals, and recovery delay.
Measure healthy Cells' p99 throughout the fault. The run must show that
publication catches up after origin recovers without discarding accepted work.
Repeat for uniform and hot/skewed workloads at 3, 5, 10, and 20 nodes, including
compatible rollout. Record executing owners during load; a pre-load owner map
cannot identify execution after migration.

## Safety and proof retained by the audit

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
