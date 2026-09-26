# Audit LTX latency and sustained publication capacity

| Document intent | Value |
| --- | --- |
| Content type | Design audit and acceptance gates |
| Audience | LTX, runtime, storage, and qualification contributors |
| Scope | Source at `0f3f4f7617a`; LTX source matches the local `origin/main` snapshot `de0bb234abc` |
| Status | Confirmed implementation costs; proposed improvements have no new latency qualification |

[Scaling plan](vfs-ltx-scale-plan.md) · [Recorded measurements](../../crab-ltx/perf/README.md)

The highest-value next experiments remove provider round trips and cache
bookkeeping from request paths. Keep the existing SQLite VFS, exact-root
verification, fencing, stable command receipts, and follower recovery model.
The recorded sub-millisecond sparse capture and roughly 87–139 ms small-root
RustFS preparation come from different harnesses and revisions. They identify
where to investigate; they cannot be subtracted to explain a public action.

## Findings in execution order

### 1. Small LTX bodies pay the multipart protocol cost

**Confirmed:** [native segment upload](../../crab-ltx/src/replica/upload.rs)
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

### 2. Persistent directory-cache hits perform durable index writes

**Confirmed:** [DirectoryCache::get](../../crab-ltx/src/environment/directory_cache.rs)
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

### 3. Sparse writable activation still reads the complete checksum directory

**Confirmed:** [prepare_writable](../../crab-ltx/src/replica.rs) awaits
[load_checksums](../../crab-ltx/src/replica/directory.rs). That function visits
every directory node sequentially, authenticates every page entry, writes
eight checksum bytes per database page, and syncs the checksum file before
opening the writer. LTX page bodies are lazy; this metadata walk is eager.
At 4 KiB pages a 10 GiB database alone needs a 20 MiB checksum file, excluding
directory transfer and validation. Tiny bootstrap Cells hide this cost.

**Change to evaluate:** first remove finding 2's cache overhead and overlap
independent authenticated node reads within existing host admission, retaining
ordered validation/output. Consider lazy checksum chunks only if this measured
walk still prevents the recovery target. That larger change must preserve the
old checksum for every overwritten/truncated page and the final root checksum.

**Gate:** measure first query and first mutation separately for fixed data
sizes, empty/warm caches, and simultaneous owner loss. Count directory requests,
checksum bytes, local syncs, admission wait, RSS, and time to serve. Exact-root
corruption, canceled activation, fresh destination, and checksum-link tests
must remain intact. A fast first page fault does not prove fast activation.

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

**Confirmed:** [start_publication](../src/cell/actor/requests.rs) removes one
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

### 7. Qualification needs a less favorable payload and arrival model

**Confirmed:** the [cost harness](../../crab-ltx/perf/replica-cost/src/main.rs)
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

## Safety and proof retained by the audit

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
