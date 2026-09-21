# Protocol v2 Stable Layered Packs

## Document metadata

| Field | Value |
| --- | --- |
| Project | Crab |
| Scope | Protocol-v2 checkpoint Git packs, clone/fetch, repack, fsck, history, and GC |
| Status | `CRBCKP05`/`CRBPKL01` implemented and re-audited; stable-prefix reuse, compact ordinal visibility, pointer-carried run-control, lazy source admission, compacted-source and frontier exact OID-to-member admission, bounded suffix range coalescing, source-selective response repack, external `REF_DELTA` propagation, thin-pack installation with fail-closed repair, direct layered-member installation with classic cold-clone selection, native connectivity-only verification, and bounded suffix-budget scheduling are live; the clean current-binary 20-commit and v2 xorb/shard E2Es are green, while long-run performance and the 5,000-commit gate remain open |
| Priority | Correctness, stable incremental cost, then clone throughput and storage efficiency |
| Replaces | Whole-repository Git-pack replacement during every v2 checkpoint |
| Companion | [Capsule Publication Protocol](capsule-publication-protocol.md), [Protocol v2 Xorb and Shard Integration](capsule-xorbs-shards.md), [Kubernetes 4,500-commit RustFS benchmark](../benchmarks/kubernetes-4500-rustfs.md) |

## 1. Decision

Protocol v2 will publish an authenticated **pack set** whose members are
immutable, content-addressed **pack sources**. A source is either one capsule
run containing a bounded directory of immutable Git pack members or one
standalone **pack layer** produced by geometric consolidation. A checkpoint
will bind the ordered source set and repository indexes, but it will not embed
or rewrite the stable pack bodies.

Foreground pushes continue to publish capsules and per-ref heads. Ordinary
checkpoint maintenance folds eligible capsule-run descriptors into the pack
set without copying their pack bytes. Repack maintenance applies a
geometric suffix policy: it rewrites only the smallest colliding sources into
one standalone layer and leaves the larger prefix byte-for-byte unchanged.

This is the required structural fix. Merely putting the current complete pack
under a separate object key would not help: the current maintenance path
installs every visible pack, calls complete-repository repack, and therefore
changes the resulting pack bytes and content identity. The read path already
skips a locally installed pack whose content identity is unchanged. Stable
pack-body identity is what converts that existing fast path into useful
incremental behavior.

The change is a hard format cutover from `CRBCKP03` to a new checkpoint and
pack-layer format. Protocol v2 is not yet a shipped storage contract, so the
release implementation will have one canonical reader and writer rather than
a permanent dual-format stack. Protocol v1 remains available and is not
retired until the qualification gates in this plan pass.

## 2. Evidence and problem statement

### 2.1 Current v2 behavior

The current checkpoint module:

1. installs every checkpoint and capsule pack visible in a pinned view;
2. runs `repack_repository_complete` across the complete reachable graph;
3. embeds the replacement pack bodies, indexes, reverse indexes, and locators
   into one `CRBCKP03` object;
4. publishes one `CheckpointPointer` to that complete object; and
5. resets the post-checkpoint frontier.

Although `CRBCKP03` can contain more than one pack, every pack body is a
section of the same checkpoint object. More importantly, complete repack
usually changes the pack content identity. An incremental client cannot reuse
its former large local pack and must read the replacement.

The September 2026 Kubernetes replay made the amplification visible. The
first 1,500 incremental pushes remained fast and request-flat, but fetches at
500-commit checkpoints took roughly 14--15 minutes and checkpoint/repack took
roughly 19 minutes. The checkpoint was about 1.18 GiB and represented about
1.49 million Git objects. Root CAS and foreground request count were not the
bottleneck; rebuilding and rereading the complete Git pack was.

### 2.2 Behavior already demonstrated by v1

The v1 Kubernetes qualification demonstrated the useful invariant: the
published pack inventory can converge while stable pack bodies remain
unchanged. Its geometric maintenance selected a small suffix, retained the
large prefix, and could report a no-op repack when the active inventory was
already geometric. That run still exposed other v1 scaling costs, but its pack
stability is the behavior v2 must preserve.

### 2.3 Success criterion

After a client has fetched checkpoint `N`, checkpoint `N + 1` must not require
that client to download, hash, or reinstall any Git pack member whose content
was already present. Maintenance cost must be proportional to the selected
suffix, not the complete repository, except for an explicitly requested full
re-optimization operation with its own budget and telemetry.

### 2.4 September 18 legacy baseline

The retained `v2-k8s-5000-20260918-150213` report sharpens the diagnosis. The
intended 5,000-commit qualification stopped after 1,500 pushes, so no later
fetch samples exist:

| Ordinal | Incremental fetch | Object-store requests | Response bytes |
| ---: | ---: | ---: | ---: |
| 500 | 864.344 s | 40 | 143.53 MiB |
| 1,000 | 893.453 s | 40 | 159.79 MiB |
| 1,500 | 896.558 s | 43 | 141.49 MiB |

The same run measured interval repacks at 1,154.600 and 1,161.526 seconds.
Each repack read about 2.4 GB and wrote about 1.18 GB through the object-store
transport. By contrast, incremental pushes through ordinal 1,500 averaged
about 393 ms and 9.01 object-store requests.

These numbers disprove a request-latency-only explanation for fetch. Forty
local RustFS requests and roughly 150 MiB cannot explain a fifteen-minute wall
time. The current remote-helper path unconditionally installs every active
checkpoint and capsule pack before validating the requested tips. The retained
client ends with one additional interval-sized pack after each 500-commit
fetch, which strongly suggests that pack fan-out, repeated validation, and
Git's post-fetch automatic maintenance dominate. The report lacks phase timers,
so that last division is an inference and must be confirmed with Git Trace2 and
Crab phase metrics before implementation claims a fix.

The audit therefore adds five mandatory corrections:

1. logical checkpoint publication must perform zero reads or writes of already
   stable pack bodies. Frontier authentication may read capsule controls, but
   the CRBCKP05 control bundle must prevent a frontier body download when its
   metadata is sufficient;
2. geometric repack must use the committed disjoint-pack concatenation fast
   path before any delta recompression fallback;
3. remote-helper incremental fetch must derive authenticated local haves and
   create one selected delta pack instead of installing the active inventory;
4. source-control reads must remain bounded, parallel, and immutable-cacheable;
   and
5. history retention must account obsolete source bytes explicitly so active
   efficiency does not hide unbounded retained storage.

### 2.5 September 18 CRBCKP04 re-audit

A fresh 20-commit first-parent slice of Kubernetes was replayed through the
rebuilt release binary against an isolated local RustFS bucket. The staging
fixture used the normal published Xet recipe path and included the
503,980,520-byte `Superset-arm64.dmg` object. Run
`crab-layered-qualification-20260918-v2-smoke-7` completed both incremental
fetches, both bounded suffix repacks, a final clone, and native full fsck; the
final tip matched the source tip.

| Operation | Wall time | Object-store requests | Result |
| --- | ---: | ---: | --- |
| Seed push | 270.7 s | 11 | passed |
| Seed layered repack | 23.6 s | 15 | passed |
| Warm incremental clone | 102.1 s | 167 | passed |
| Fetch after 10 pushes | 27.6 s | 19 | passed |
| Suffix repack after 10 | 34.1 s | 27 | passed |
| Fetch after 20 pushes | 30.7 s | 20 | passed |
| Suffix repack after 20 | 36.0 s | 28 | passed |
| Final clone | 136.3 s | 166 | passed |

The fetch elapsed time is the remote-helper `git fetch` phase; the harness
then ran `git fsck --connectivity-only` separately. The final clone ran
`git fsck --full` and passed. This establishes the current v2 behavior as
correct and bounded, but not yet at the release performance target: the two
warm fetches are above the ten-second p95 target and use 19 and 20 origin
operations. Sidecar coalescing is implemented, but this workload still used
14 and 15 v2 GETs because its selected member ranges were not adjacent enough
to merge. The first fetch transferred 125.8 MiB and the second 126.9 MiB;
local response-pack generation, source validation, and pack materialization
were CPU-bound and dominated wall time. Exact reuse now bypasses that work when
the selected object set equals one complete layered member and every external
delta base is in the authenticated have set. The fail-closed complete-member
union path is now implemented, but this smoke predates that path and does not
claim its latency improvement. A 20-commit smoke cannot
qualify the 500-commit or 5,000-commit trend; that gate remains open.

Foreground push measurements show the same split. Ordinary small commits in
this run were 446--653 ms and exactly eight object-store operations. The large
Xet pointer transition reached 20.1 s/58 operations, and the following
pointer-only transition reached 16.5 s/37 operations. Across all 20
incremental pushes the mean was 2.29 s and 11.95 operations, so the under-ten
mean gate is not met for this mixed large-file slice even though the
small-commit p50 was 502 ms and eight operations.

The first 500-commit interval gives the required long-window baseline before
the complete-member union change. With the pre-union release binary, the
incremental fetch completed with tip/connectivity verification in 1,027,383 ms
(17.1 minutes), using 43 origin operations: 28 v2 GETs, two v2 LISTs, twelve
lock writes, and one replica-discovery GET. It transferred 210.9 MiB. The
interval repack was still running during this audit, so this is a fetch-only
baseline; it is already enough to disprove the ten-second target and to show
that local response-pack materialization, not object-store request latency, is
the dominant v2 cost at this scale.

The latest rebuilt-binary slice,
`crab-layered-qualification-20260918-v2-incremental-smoke-20-cp04`, is the
current v2 read measurement. It supersedes the older smoke-7 timing table above
for the hot-fetch comparison; both runs passed their stated correctness checks.

| Operation | Current wall time | Origin operations | Object-store response bytes |
| --- | ---: | ---: | ---: |
| Fetch after 10 pushes | 27.65 s | 19 | 125.8 MiB |
| Suffix repack after 10 | 31.76 s | 27 | 213.8 MiB |
| Fetch after 20 pushes | 28.45 s | 20 | 126.9 MiB |
| Suffix repack after 20 | 31.75 s | 28 | 218.1 MiB |

The current slice also measured a 160.2 s warm incremental clone and a 159.5 s
final clone. It is a 20-commit smoke, not a long-run trend: it does not replace
the 500-commit baseline or prove the open 5,000-commit release gate.

### 2.5.1 September 18 CRBCKP05 control-bundle re-audit

The rebuilt CP05 binary was then run against a fresh 20-commit Kubernetes-derived
first-parent fixture in isolated local RustFS. The fixture is intentionally
bounded (about 16 MiB of working tree and 5.8 MiB of Git data); it is a smoke
fixture, not the full Kubernetes qualification. Seed, both interval repacks,
both incremental fetches, final clone, and full native fsck passed, and the
final tip matched the source tip.

| Operation | Wall time | Object-store requests | Result |
| --- | ---: | ---: | --- |
| Seed push | 1.118 s | 11 | passed |
| Seed layered repack | 0.215 s | 15 | passed |
| Warm incremental clone | 0.749 s | 16 | passed |
| Fetch after 10 pushes | 0.253 s | 49 | passed |
| Suffix repack after 10 | 0.385 s | 27 | passed |
| Fetch after 20 pushes | 0.262 s | 50 | passed |
| Suffix repack after 20 | 0.403 s | 28 | passed |
| Final clone | 0.756 s | 21 | passed |

The run-control bundle removed the per-capsule transaction/visibility/catalog
range fan-out: CP05 fetches fell from the prior 69/73 requests to 49/50 and
from 284/302 ms to 253/262 ms on the same fixture. The 20 incremental pushes
remained flat at 299.3 ms mean (296 ms p50, 317 ms p95) and exactly eight
object-store operations each. The fetch target is still not met: 49/50 total
origin operations are well above the warm single-ref budget of ten. The
remaining operations were dominated by eager source-sidecar admission and
control discovery in that pre-change binary, not by the generated response
pack. The current reader removes that eager full-sidecar wave; transition-
driven frontier selection is still required for the final request bound. This
is a correctness pass and a measured improvement, not a performance-release
qualification.

### 2.5.2 September 19 lazy-admission and direct-control re-audit

A rebuilt `crab 1.2.4` binary was replayed as
`crab-layered-cp05-reaudit-20260919` against an isolated local RustFS bucket
using the bounded Kubernetes-derived fixture. The run completed the seed push,
20 incremental pushes, both incremental fetches, both suffix repacks, a fresh
clone, and native full fsck; the final tip matched the source tip.

| Operation | Wall time | Total object-store requests | Object-store response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 1.256 s | 11 | — | passed |
| Seed suffix repack | 0.228 s | 15 | 2.05 MiB | passed |
| Fetch after 10 pushes | 0.259 s | 41 | 82.3 KiB | passed |
| Suffix repack after 10 | 0.330 s | 27 | 173.0 KiB | passed |
| Fetch after 20 pushes | 0.258 s | 39 | 87.0 KiB | passed |
| Suffix repack after 20 | 0.342 s | 28 | 190.9 KiB | passed |
| Final clone | 0.898 s | 22 | 1.92 MiB | passed |

The two incremental fetches used 33 and 34 v2 GETs respectively, plus two
metadata LISTs, two or five read-admission lock calls (the first fetch had a
lease-reconciliation retry), and one replica-discovery GET. The small-commit
push stream stayed at exactly eight operations per push, with 308.95 ms mean,
307 ms p50, and 323 ms p95 (328 ms maximum). Fetch response bytes are already
delta-sized; the unchanged 39--41 request count is still frontier
source-control and sidecar admission rather than payload transfer. The range
coalescer and external-base closure are therefore correctness-ready and
bounded, but this fixture does not demonstrate a request-count reduction. This
is not a claim that the ten-operation warm-fetch gate or the 5,000-commit gate
has passed. The bounded fixture is also too small to predict hosted WAN
latency.

The repack implementation coalesces selected member body/sidecar ranges per
immutable source under the same 64 KiB gap, 4 MiB extra-byte, and 16 MiB window
limits used by the read planner. Consolidated output carries the exact
target-to-base map for generated `REF_DELTA` entries; external bases are read
from the pinned layered view and verified before suffix publication. Thus the
structural concatenation path remains the default for disjoint complete packs,
while cross-source delta repair is bounded and fail-closed.

### 2.5.3 September 19 source-aware range-coalescing re-audit

The same CP05 fixture was replayed with the then-current binary ref-run fan-in
two and the source-aware reader coalescer enabled as
`crab-layered-cp05-coalesced-fanin2-20260919`. Seed, 20 incremental pushes,
both fetch/repack checkpoints, a final clone, and native full fsck passed; the
final tip matched the expected source tip.

| Operation | Wall time | Total object-store requests | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 1.170 s | 11 | 2.00 MiB | passed |
| Seed suffix repack | 0.210 s | 15 | 2.05 MiB | passed |
| Warm incremental clone | 0.854 s | 16 | 1.82 MiB | passed |
| Fetch after 10 pushes | 0.252 s | 14 | 121.4 KiB | passed |
| Suffix repack after 10 | 0.225 s | 17 | 168.6 KiB | passed |
| Fetch after 20 pushes | 0.259 s | 14 | 129.7 KiB | passed |
| Suffix repack after 20 | 0.410 s | 21 | 212.7 KiB | passed |
| Final clone | 0.917 s | 22 | 1.92 MiB | passed |

The coalescer reduced each warm fetch from 22 requests/14 range GETs to
14 requests/6 range GETs on this fixture. It joins selected payload ranges
from distinct pack members that share one immutable capsule-run object while
preserving each member's local offset for CRC, delta-base, and object-ID
validation. The small-commit push stream remained flat: 9.8 requests mean,
8 p50, 13 p95, 312.55 ms mean, 306 ms p50, and 334 ms p95. This is the best
current bounded v2 fetch measurement, not a 500- or 5,000-commit qualification:
the ten-operation warm-fetch gate, full-Kubernetes trend, and hosted-WAN gate
remain open. The full-repository replay also exposed and fixed a separate
production-size seed failure: large visibility/catalog controls now detach
from the run footer and are range-fetched with hash verification instead of
being rejected by the 8 MiB footer bound.

### 2.5.4 September 19 full-Kubernetes CP05 qualification slice

The detached-control fix was then exercised against the full Kubernetes
checkout for 100 first-parent commits with local RustFS. The seed push and
four 20-commit windows completed the protocol path; the fifth window could not
start because the replay input reached a 503,980,520-byte Xet pointer without
the required clean staging source. That is a qualification-input failure, not
a layered-pack or fetch-integrity failure, so this run is not counted as a
100-commit pass.

| Operation | Wall time | Total object-store requests | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 227.6 s | 11 | — | passed |
| Seed suffix repack | 18.4 s | 15 | 1.36 GiB | passed |
| Warm incremental clone | 105.6 s | 166 | 1.26 GiB | passed |
| Fetch after 20 pushes | 40.1 s | 21 | 34.1 MiB | passed |
| Fetch after 40 pushes | 39.7 s | 19 | 35.0 MiB | passed |
| Fetch after 60 pushes | 40.4 s | 16 | 32.6 MiB | passed |
| Fetch after 80 pushes | 40.2 s | 20 | 34.2 MiB | passed |
| Suffix repacks at 20/40/60/80 | 41.2--42.7 s | 17--21 | 96--105 MiB | passed |

The four completed fetches stayed in a narrow 39.7--40.4 s band and 16--21
operations while returning 32.6--35.0 MiB. This is the current production-size
v2 incremental-fetch measurement. The stable-prefix design prevents a full
repository object-store rewrite, but response-pack transfer and local Git
pack/index materialization still dominate wall time at this scale; the bounded
fixture's sub-second fetch is not predictive of this workload. The full
Kubernetes 100/100 and 5,000-commit gates remain open until the same replay is
rerun with a clean canonical staging source and completes clone, fetch,
repack, fsck, and Xet/shard checks.

### 2.5.5 September 19 staged full-source smoke

To separate the missing-staging input from the protocol, the same full source
was replayed for 20 commits with a clean canonical staging snapshot. The run
passed both incremental fetches, both suffix repacks, final clone, tip
equality, and native full fsck, including the large Xet pointer transition.

| Operation | Wall time | Total object-store requests | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 205.6 s | 11 | 1.30 GiB | passed |
| Seed suffix repack | 18.0 s | 15 | 1.36 GiB | passed |
| Warm incremental clone | 79.6 s | 167 | 1.26 GiB | passed |
| Fetch after 10 pushes | 20.9 s | 16 | 32.5 MiB | passed |
| Suffix repack after 10 | 22.6 s | 17 | 95.6 MiB | passed |
| Fetch after 20 pushes | 21.4 s | 19 | 33.6 MiB | passed |
| Suffix repack after 20 | 23.2 s | 21 | 100.4 MiB | passed |
| Final clone | 123.7 s | 168 | 1.32 GiB | passed |

The 20 ordinary incremental pushes were 345--851 ms except for the Xet
pointer transitions (13.4 s and 28.3 s); their overall mean was 2.47 s and
13.1 operations. The large-file cost is therefore not evidence that stable
Git pack layers are being rewritten. It is the required Xet staging/read path,
which remains separately qualified alongside pack correctness.

### 2.5.6 September 19 request-budget re-audit

The writer path was re-audited after the layered-pack fetch work exposed an
avoidable admission cost. A push-only ref snapshot now loads each selected ref
once; publication still re-reads the head, checks the expected old OID, and
commits with its ETag CAS, so the optimization does not weaken race or
root-epoch protection. Production ref-run compaction remains binary: it keeps
the frontier shallow enough for incremental fetch while bounding each merge
wave. Layered repack/checkpoint still folds the accumulated suffix before a
large fetch or clone, so foreground pushes do not rewrite the stable prefix.

The capsule request regression now measures ten object-store operations for the
second simple push on the generic in-memory contract store (the assertion is
an upper bound of ten to keep the contract backend-independent). The 20-push
CP05 stream remains below ten requests on average because most pushes do not
trigger a merge wave. This is a request-budget improvement, not a claim that
every provider returns ten requests: generic stores may require immutable
readback verification, while checksum-capable providers can use the cheaper
verified-write path.

Current v2 incremental-fetch envelope remains workload-shaped:

| Fixture | Fetch wall time | Origin operations | Response bytes | Repack wall time |
| --- | ---: | ---: | ---: | ---: |
| CP05 bounded, 20 commits | 252--259 ms | 14 | 121--130 KiB | 225--410 ms |
| Kubernetes-derived, 100 commits (through fetch 80) | 39.7--40.4 s | 16--21 | 32.6--35.0 MiB | 41.2--42.7 s |
| Full Kubernetes source, staged 20 commits | 20.9--21.4 s | 16--19 | 32.5--33.6 MiB | 22.6--23.2 s |
| Full Kubernetes source, frontier-candidate 20 commits (fresh RustFS) | 25.82--35.82 s | 22--27 | 38.6--39.7 MiB | 25.59--26.02 s |

The small fixture is the best request-count result, not a production-size
latency prediction. The large runs show bounded request counts but high local
response-pack/materialization cost; the fresh frontier-candidate run stays at
22--27 operations, while the earlier shared-server sample was 22--24 and is
not a latency benchmark. The 5,000-commit and hosted-WAN gates are still open.
The layered-pack design therefore has a valid bounded-repack strategy and a
correctness-passing fetch path, but it is not yet qualified to retire v1 for
large repositories.

### 2.5.7 September 19 current Kubernetes 500-commit baseline

The current release binary was replayed against the read-only Kubernetes
first-parent history with the canonical Xet staging snapshot and an isolated
local RustFS prefix. Seed push, seed layered repack, incremental clone, and the
first 500 pushes all passed tip and connectivity checks. The checkpoint-500
fetch is the current v2 production-shaped read measurement:

| Operation | Wall time | Origin operations | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Incremental fetch at 500 | 861.883 s (14.36 min) | 143 (130 v2 GETs, 2 LISTs, 10 lock PUTs, 1 replica GET) | 83,061,905 (79.2 MiB) | passed |
| Suffix repack at 500 | 1,125.235 s (18.75 min) | 23 | 147,828,487 (141.0 MiB) | passed |
| Incremental fetch at 1,000 | 1,084.939 s (18.08 min) | 161 (146 v2 GETs, 2 LISTs, 12 lock PUTs, 1 replica GET) | 98,670,725 (94.1 MiB) | passed |
| Incremental fetch at 1,500 | 912.1 s (15.20 min) | 146 | 78,366,148 (74.7 MiB) | passed |
| Incremental fetch at 2,000 | 1,026.836 s (17.11 min) | 171 (156 v2 GETs, 2 LISTs, 12 lock PUTs, 1 replica GET) | 99,662,403 (95.0 MiB) | passed |
| Incremental fetch at 2,500 | 1,023.870 s (17.06 min) | 161 | 86,889,965 (82.9 MiB) | passed |
| Incremental fetch at 3,000 | 959.676 s (15.99 min) | 147 | 93,777,933 (89.4 MiB) | passed |
| Incremental fetch at 3,500 | 1,094.396 s (18.24 min) | 188 | 101,258,459 (96.5 MiB) | passed |
| Suffix repack at 3,500 | 1,201.648 s (20.03 min) | 24 | 284,452,668 (271.2 MiB) | passed |

The 500--3,500 rows are the long-running pre-frontier-join baseline artifact;
they are retained because they are the only production-shaped trend so far.
The exact join artifact has only the bounded 20-commit result in 2.5.11, so it
must not be presented as a large-repository improvement until an uncontended
500/5,000-commit replay completes.

The fetch process remained CPU-bound while the RustFS service stayed healthy;
the request count and response bytes are far too small to explain fourteen
minutes of elapsed time. This confirms that response-pack construction,
Git/index-pack validation, and local materialization dominate this workload.
The 500-to-1,000 interval increased to 18.08 minutes and 161 operations even
though the response was only 94.1 MiB; the 1,500 sample fell back to 15.20
minutes and 146 operations, but remains orders of magnitude above the target
and is not flat enough for a release claim. The still-running baseline reached
ordinal 3,500 with an 18.24-minute fetch (188 operations); its 3,500 repack
took 20.03 minutes and transferred 271.2 MiB. The current v2 path is
therefore not yet flat over commit count.
The repack is bounded to the selected layered suffix and does not read the
stable prefix, but a large selected source still incurs a large one-time
read. The scheduler now defers an over-budget geometric merge and forces only
the minimum suffix needed when the 64-source format bound would otherwise be
exceeded. Repack is therefore a background maintenance operation, not a
foreground-push latency contract.

The optimized binary adds source-selective response repacking: it resolves the
selected object locators plus authenticated `REF_DELTA` closure first, then
downloads only the source packs containing that closure. It fails closed on a
missing locator, an unknown source pack, a cancellation, or an incomplete
closure; it never silently substitutes an incomplete pack. The optimized
20-commit Kubernetes-derived smoke passed all clone/fetch/repack/fsck checks
with 271--290 ms fetches and 14 origin operations (release binary
`a7898c86bdbea659e27a22c4ed1babe185679a286a7d0b155d004d1a9833012d`). A fresh 500-commit
latest-tail run has now recorded a correctness-passing fetch (see 2.5.8), but
its suffix repack and final clone are still in progress. It is not an
apples-to-apples before/after comparison with the older-history baseline, so no
large-workload improvement is claimed.

### 2.5.8 September 19 latest-tail source-selective fetch

The source-selective response-pack binary was also exercised against the
latest 500 first-parent Kubernetes commits using the canonical staged Xet
snapshot. Seed publication and the initial layered clone passed. The ordinal
500 fetch passed tip and connectivity verification and measured:

| Operation | Wall time | Origin operations | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Incremental fetch at 500 | 992.183 s (16.54 min) | 217 (203 v2 GETs, 2 LISTs, 11 lock PUTs, 1 replica GET) | 135,698,392 (129.4 MiB) | passed |

This is a current v2 latest-tail envelope, not a before/after claim: the
5,000-commit baseline above starts 5,000 commits behind `HEAD`, so its first
500 commits are a different history window. The run's large Xet transition
also made ordinal 499/500 pushes take 858.593 s and 728.692 s; those writes are
staged large-file publication, not layered-pack fetch. The fetch result still
fails the latency and warm-fetch request targets, confirming that source
selection alone does not remove the local response-pack reconstruction and
Git/index-pack cost. The current reader no longer fetches every frontier
index/reverse/locator window during repository open: frontier pack indexes are
admitted lazily and reverse/locator sidecars remain deferred to explicit
install/repack paths. This older measurement predates that change, so it is
not a post-change benchmark; authenticated transition-to-member admission,
dense response assembly, and the 5,000-commit qualification remain release
gates. No v1 retirement or large-repository performance claim is justified
from this run.

### 2.5.9 September 19 post-lazy-admission full-source smoke

The rebuilt binary containing lazy frontier index admission and bounded suffix
scheduling was run as `crab-layered-cp05-lazy-20260919-r2` against the same
full Kubernetes source and canonical staged Xet snapshot. Seed publication,
both incremental fetches, both suffix repacks, final clone, tip equality, and
native full fsck passed. A separate 5,000-commit replay was concurrently
using the same local RustFS service, so these timings are correctness evidence
and a post-change envelope, not an uncontended benchmark.

| Operation | Wall time | Origin operations | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 386.3 s | 11 | 1.30 GiB | passed |
| Seed suffix repack | 10.8 s | 15 | 1.36 GiB | passed |
| Warm incremental clone | 107.8 s | 167 | 1.25 GiB | passed |
| Fetch after 10 pushes | 21.9 s | 23 | 76.3 MiB | passed |
| Suffix repack after 10 | 23.4 s | 17 | 95.6 MiB | passed |
| Fetch after 20 pushes | 22.0 s | 38 | 77.4 MiB | passed |
| Suffix repack after 20 | 23.6 s | 21 | 100.4 MiB | passed |
| Final clone | 155.3 s | 168 | 1.26 GiB | passed |

The 20 incremental pushes had a 547 ms p50, 2.14 s mean, and 13.1-request
mean; the p95 was 15.9 s because the staged large-file transitions are Xet
publication work, not Git layered-pack rewrites. The post-change reader no
longer eagerly downloads every frontier reverse/locator/kind sidecar, but the
full-source fetch remains above the ten-operation and ten-second warm-fetch
targets. Transition-to-member admission and an uncontended 5,000-commit run
are still required before v2 can replace v1 as the large-repository baseline.

### 2.5.10 September 19 exact-current-binary bounded fixture

The exact release artifact used for the implementation checks
(`634b4ada790cb1a35336e9cf4faf8d766f3254b89b861f5e06802ef26ae2e943`) was
also run without the concurrent full-source workload against the 16 MiB
Kubernetes-derived fixture. Seed, two incremental fetches, two repacks, final
clone, tip equality, and native full fsck passed.

| Operation | Wall time | Origin operations | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Fetch after 10 pushes | 319 ms | 23 | 136.1 KiB | passed |
| Suffix repack after 10 | 378 ms | 17 | 168.5 KiB | passed |
| Fetch after 20 pushes | 360 ms | 33 | 155.4 KiB | passed |
| Suffix repack after 20 | 628 ms | 21 | 212.8 KiB | passed |

The 20 ordinary pushes remained under one second at p50 (500 ms) with a
676 ms mean and 9.8-request mean. This is the clean current-code smoke
envelope; it demonstrates sub-second small-repository fetch latency, not a
large-repository promise. The remaining request-count gap is the missing
authenticated transition-to-source/member join, while the large-source gap
is local response-pack materialization and Git index-pack CPU.

### 2.5.11 September 19 authenticated compacted-admission re-audit

The exact release artifact containing the ordinal-to-source/member admission
join (`d2d43339d8fa7fc57ed41bd31ff9bc4e3b233035af35f3b6bf8501898a88982c`)
was then run against a dedicated RustFS prefix with the same staged
Kubernetes-derived source. Seed publication, both fetch/repack checkpoints,
the final clone, tip equality, and native full fsck passed. The endpoint used
the long-run server's alternate listener, so these timings are correctness and
request-shape evidence under server contention, not an uncontended latency
benchmark; section 2.5.12 is the clean measurement.

| Operation | Wall time | Origin operations | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 195.0 s | 11 | 1.30 GiB | passed |
| Seed suffix repack | 10.8 s | 15 | 1.37 GiB | passed |
| Warm incremental clone | 68.7 s | 167 | 1.26 GiB | passed |
| Fetch after 10 pushes | 21.4 s | 23 (18 v2 GETs) | 82.6 MiB | passed |
| Suffix repack after 10 | 22.1 s | 17 | 114.4 MiB | passed |
| Fetch after 20 pushes | 21.5 s | 35 (30 v2 GETs) | 83.7 MiB | passed |
| Suffix repack after 20 | 22.6 s | 21 | 119.3 MiB | passed |
| Final clone | 125.0 s | 168 | 1.27 GiB | passed |

This run proves the new admission table is authenticated and does not regress
the end-to-end read or fsck contract, but it does not yet meet the performance
target. The table covers objects in the compacted checkpoint visibility
dictionary. Objects introduced by the post-checkpoint frontier were still
represented by run-level pack descriptors without an exact OID-to-member join
in this artifact, so the reader had to probe frontier indexes before it could
narrow the selected object set. That is why the v2 GET count remained high
even though stable checkpoint members were admitted selectively. The remaining
design work is a bounded, exact frontier admission sidecar (or equivalent
run-level join), not a weaker Bloom-filter authorization shortcut. The
5,000-commit and hosted-WAN gates remain open.

### 2.5.12 September 19 frontier-candidate admission re-audit

The fail-closed frontier candidate admission path was then exercised with
release artifact `93caba8f508997aa6f9ce034bac649113615cb21604e6a8ff9b6b5413239ca7b`
(`crab 1.2.4`) against a genuinely separate RustFS server (API port 9100,
fresh data root) and the full staged Kubernetes source. The path authenticates
frontier visibility deltas and joins each newly visible object to the candidate
pack members in its capsule run. A missing or ambiguous candidate falls back
to the complete pinned inventory, so the optimization cannot hide a required
object. Seed publication, both fetch/repack checkpoints, final clone, tip
equality, and native full fsck all passed.

| Operation | Wall time | Total object-store requests | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 258.1 s | 11 | 1.30 GiB | passed |
| Seed suffix repack | 12.3 s | 15 | 1.37 GiB | passed |
| Warm incremental clone | 123.6 s | 167 | 1.26 GiB | passed |
| Fetch after 10 pushes | 35.82 s | 22 (17 v2 GETs) | 38.6 MiB | passed |
| Suffix repack after 10 | 26.02 s | 17 | 114.4 MiB | passed |
| Fetch after 20 pushes | 25.82 s | 27 (19 v2 GETs) | 39.7 MiB | passed |
| Suffix repack after 20 | 25.59 s | 21 | 119.3 MiB | passed |
| Final clone | 202.3 s | 168 | 1.27 GiB | passed |

The 20 incremental pushes averaged 6.11 s and 13.65 object-store requests;
the p50 was 1.17 s. The staged Xet pointer transitions were the outliers
(15.2 s and 77.0 s, with the latter reaching 62 requests and retrying eleven
transient 5xx responses); ordinary Git pushes after the seed stayed in the
sub-second-to-low-single-digit range. The frontier join keeps the clean fetch
request shape at 22--27 operations, but
the 25.8--35.8 s wall time remains dominated by local response-pack
generation and `index-pack` CPU/storage work after the object-store reads.
This is a correctness and bounded-request improvement, not evidence that the
ten-operation or ten-second warm-fetch targets have passed.

The earlier run in this section's shared-server prefix measured 22--24
operations and 21.8--22.8 s, but it is not used as a latency claim because it
shared RustFS with the long replay. The fresh-server result above is the
authoritative current v2 incremental-fetch envelope.

The previous candidate map was intentionally run-level: every
visibility-added object admitted all members in its authenticated run. The
current `CRBRUN04` exact sidecar replaces that over-admission with an
authenticated OID-to-member join, so control-only fetch can pick one member
without opening the other indexes. A probabilistic filter is not an
acceptable substitute. Until the uncontended 5,000-commit replay passes, v1
remains the large-repository performance baseline.

### 2.6 Layered-pack efficiency audit

The current `CRBCKP05` implementation has two different performance paths
that must not be conflated:

* The pack-set/repack path is structurally layered. A metadata-only checkpoint
  carries immutable capsule-run and pack-layer descriptors. When a geometric
  suffix is selected, `consolidate_pack_suffix` first attempts byte-preserving
  concatenation for disjoint complete packs, then falls back to selected-pack
  `git pack-objects`. The stable prefix is not downloaded or rewritten.
* The CP05 foreground path is metadata-minimal with respect to visibility and
  frontier bodies. The ordinal proof is bound to the source catalog digest,
  and `CRBRUN04` carries a bounded authenticated transaction/control bundle
  plus an exact OID-to-member admission sidecar in its suffix. Oversized
  visibility/catalog sections stay as committed ranges
  and are fetched only when that control is needed. A warm read therefore does
  not fetch a full frontier run or one range per nested control section.
* Stable and frontier layered-source sidecars are now admitted lazily. The
  ordinary read path fetches only the authenticated pack index needed to probe
  a preferred frontier member; reverse indexes and kind/locator sidecars stay
  cold until explicit pack installation or repack. Frontier controls remain
  eager because they are the mutable visibility boundary; oversized controls
  are still range-addressed and hash-verified rather than copied into the hot
  footer. The compacted checkpoint visibility dictionary carries an exact
  ordinal-to-source/member admission table. Post-checkpoint frontier runs now
  carry a fail-closed exact OID-to-member join, so the reader opens only
  indexes that can contain a visibility-added object. Missing or unparseable
  sidecars fall back to all authenticated members and can add requests, never
  hide a required object.
  The pre-sidecar isolated full-source baseline measured 22 and 27 total
  origin operations (17 and 19 v2 GETs) for 38.6--39.7 MiB responses. The
  release exact-sidecar run measured 24 and 26 total operations (20 and 22
  v2 GETs); the bounded fixture still reads 9 v2 GETs (14 total origin
  operations) for 121--130 KiB. The ten-operation warm-fetch target is not
  met because detached control and response-pack reads still dominate the
  request shape.
* Response-pack generation now selects its source inventory from the requested
  locators and recursively authenticated `REF_DELTA` bases before downloading
  source bodies. This prevents a dense partial fetch from reading every stable
  source merely because the selected object set is not a complete member. The
  complete-member and concatenation paths remain ahead of this fallback, and
  every path proves the exact requested object universe before installation.

The earlier catalog shortcut is explicitly rejected. The v2 reader creates a
synthetic manifest over capsule sources; it is not bound to the v1 SlateDB
catalog checkpoint. Treating it as a v1 catalog fails closed with a visibility
identity mismatch. The compact ordinal proof and control bundle are now the
canonical CP05 path; the remaining performance work is source-local admission,
not a weaker authorization shortcut.

### 2.5.13 September 19 exact-admission release E2E

The hard-cutover `CRBRUN04` implementation was then run with release binary
`4b4a320819f29fb5f372caf5ca7a26339c1d0ba599e03751e140f2244b77bfbd`
(`crab 1.2.4`) against a fresh RustFS namespace and the same staged
Kubernetes source. The run passed both incremental fetches, both interval
repacks, final tip equality, and native full fsck. The exact sidecar reduced
frontier admission to the member ordinals proven by the sorted OID map; all
missing or malformed sidecars still fail closed to the authenticated complete
inventory.

| Operation | Wall time | Total object-store requests | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 210.3 s | 11 | 1.34 GiB | passed |
| Seed suffix repack | 12.3 s | 15 | 1.41 GiB | passed |
| Warm incremental clone | 78.5 s | 167 | 1.17 GiB | passed |
| Fetch after 10 pushes | 21.75 s | 24 (20 GETs) | 38.6 MiB | passed |
| Suffix repack after 10 | 22.61 s | 17 | 114.4 MiB | passed |
| Fetch after 20 pushes | 23.01 s | 26 (22 GETs) | 39.7 MiB | passed |
| Suffix repack after 20 | 23.70 s | 21 | 119.3 MiB | passed |
| Final clone | 154.2 s | 171 | 1.17 GiB | passed |

The 20 incremental pushes averaged 2.48 s and 13.25 object-store requests;
the p50 was 0.485 s and the p95 13.83 s. Eighteen ordinary Git/Xet-free
pushes stayed between 0.395 s and 1.096 s (8--13 requests). The two staged
Xet pointer transitions were the outliers at 13.83 s/36 requests and
26.33 s/52 requests, including multipart uploads and retry probes. The run
therefore proves correctness and stable ordinary-push latency, but not the
under-10-request average or a sub-10-second fetch target. The fetch wall time
is still dominated by local response-pack generation and `index-pack` CPU after
the authenticated source ranges have been read.

The high-efficiency design has three remaining release gates:

1. Qualify the exact frontier sidecar on the fresh 5,000-commit replay. Keep
   reverse indexes and kind metadata cold on the normal fetch path; a
   probabilistic filter alone is insufficient because a false positive may
   select the wrong source and hide a required object.
2. Keep the external `REF_DELTA` closure on every history, restore, and GC
   path, and add long-run tests for cross-source bases and structural suffix
   replacement.
3. Run the fresh 5,000-commit Kubernetes qualification with stable-prefix
   body-read, request, byte, CPU, RSS, repack, clone, fsck, and xorb/shard
   gates.

The re-audit makes the first gate concrete. CP05 carries an authenticated
transition-to-source/member admission index and `CRBRUN04` carries the exact
frontier OID-to-member join, so opening a repository does not probe every
frontier index before upload-pack has computed wants minus haves. The reader
loads only the selected frontier indexes, feeds those ranges to the existing
exact-pack, complete-member, and selected-closure ladder, and exposes phase metrics for
`control_admission`, `locator_resolution`, `range_read`, `delta_materialize`,
`pack_write`, and local `index-pack`. Repack scheduling now defers a geometric
roll-up above the 512 MiB selected-suffix budget while staying below the
64-source correctness limit, and uses the disjoint concatenation path whenever
the selected suffix permits it. The frontier join and response materialization
work are still required before the stable-prefix format can honestly promise
flat large-repository fetch latency.

This is a hard v2 format contract, not a cache hint or an unchecked Bloom
filter. The release gate is a fresh 5,000-commit
qualification showing stable-prefix body reads of zero, fetch response bytes
proportional to the commit delta, and p95 warm-fetch latency below the stated
target. Until that run passes, v1 remains the production performance baseline
and v2 should be described as correctness-complete but not performance-ready.

### 2.5.14 September 19 v2 xorb/shard end-to-end qualification

The release binary used for the exact-admission run was also exercised by the
isolated `layered-v4-xet-20260919-r6` RustFS smoke. This is a clean synthetic
large-file fixture rather than a Kubernetes history benchmark; it isolates the
v2 Xet path so a pre-existing staging database cannot mask pack or payload
behavior.

All 33 checks passed. The run proved one canonical xorb and one canonical
shard were uploaded for two identical 4 MiB files, the v2 root and capsule
were published, a fresh clone reached the expected commit, hydration restored
both files byte-for-byte, and dehydrate/rehydrate preserved the same bytes.
The SQLite chunk index was used and no legacy redb cache was created. The
malformed non-v1 prefix probe also returned `CRAB-E0020` without creating a
manifest or mutating the prefix. This closes the v2 xorb/shard correctness
smoke; it is not evidence that large-repository fetch latency has passed the
open 5,000-commit gate.

### 2.5.15 September 19 release-artifact clean-bucket replay

The qualification release binary (`crab 1.2.4`, SHA-256
`600cc3e28b6eb024457aecf1065d0b2323408e566166777d75d8be6da8a516df`) was
replayed against a genuinely empty RustFS bucket. The read-only Kubernetes
source and canonical staged Xet recipe were unchanged; unlike the earlier
retry experiment, this run had no pre-existing repository prefix or shared
xorb authority. It completed 20 incremental pushes, fetches at pushes 10 and
20, both suffix repacks, a second clone, and native full fsck. The final
remote tip matched the source tip. This is the current correctness baseline
for the thin-response implementation.

| Operation | Wall time | Total object-store requests | Response bytes | Result |
| --- | ---: | ---: | ---: | --- |
| Seed push | 255.4 s | 11 | 1.34 GiB | passed |
| Seed suffix repack | 24.2 s | 15 | 1.41 GiB | passed |
| Warm incremental clone | 123.2 s | 167 | 1.26 GiB | passed |
| Fetch after 10 pushes | 21.95 s | 27 | 38.6 MiB | passed |
| Suffix repack after 10 | 23.39 s | 17 | 114.4 MiB | passed |
| Fetch after 20 pushes | 22.69 s | 26 | 39.7 MiB | passed |
| Suffix repack after 20 | 24.96 s | 21 | 119.3 MiB | passed |
| Final clone | 112.2 s | 168 | 1.27 GiB | passed |

The 20 incremental pushes averaged 2.176 s and 13.25 object-store requests;
the p50 was 0.402 s, p95 13.677 s, and maximum 21.796 s. Ordinary commits
remained 0.346--0.454 s with 8--13 requests. The two staged pointer/Xet
transitions were the outliers at 21.796 s/52 requests and 13.677 s/36
requests. Both clean-bucket attempts succeeded on their first publication;
the earlier integrity error was therefore isolated to a reused namespace and
must remain covered by corruption/retry tests rather than being waived.

The fetch path now derives local have tips, plans `wants - authenticated
haves`, and emits one response pack. For a complete non-shallow,
non-promisor repository it may retain proven haves as external `REF_DELTA`
bases; local installation invokes Git `index-pack --fix-thin --stdin` and
atomically publishes the `.pack`/`.idx`/`.rev` sidecars. Shallow, partial, or
unreadable-config repositories use the self-contained response path. A
missing base, malformed sidecar, or failed index operation leaves no final
pack and fails closed. This fixes correctness for the thin path, but the
measured 21.95--22.69 s fetches and 26--27 origin operations show that
response-pack generation and local Git validation still dominate; the
ten-operation and ten-second targets remain open.

After this replay, the release artifact was rebuilt with the async-frame
hardening that keeps the large layered planner off the legacy fetch frame
(`crab 1.2.4`, SHA-256
`96b44736e4a1364ddac999de9616ad91a7e45ce61113e363e58447dfc2f79bfe`). The
full remote-helper suite passes with that change; the clean-bucket measurements
above remain attributed to the artifact that produced them.

### 2.5.16 September 19 current-artifact replay

The rebuilt artifact (`crab 1.2.4`, SHA-256
`96b44736e4a1364ddac999de9616ad91a7e45ce61113e363e58447dfc2f79bfe`) was
replayed from an empty local RustFS bucket under a fresh namespace. The run
completed all 20 pushes, fetched and repacked at pushes 10 and 20, cloned the
final state, matched the source tip, and passed full fsck. Its measured fetches
were 22.60 s with 24 requests after push 10 and 22.15 s with 27 requests after
push 20; responses were 38.6 MiB and 39.7 MiB respectively. One transient
5xx was retried during the second fetch and did not change the authenticated
result. This confirms the performance observation on the exact rebuilt
artifact, while the ten-operation and ten-second targets remain open.

The response producer now applies the same structural-union fast path to a
negotiated thin response. When authenticated locators prove that the selected
objects partition into two or more complete immutable members, each member's
index must prove its `REF_DELTA` bases against the union of selected objects
and authenticated local haves; only then are the raw member bodies
concatenated. An overlap, missing member, sidecar, or base proof falls back to
the bounded response writer, so this optimization cannot weaken fetch
correctness. The cross-pack `REF_DELTA` closure is covered by a Git
`index-pack --fix-thin`/`cat-file` regression test; the current 20-commit
artifact measurements predate this change, so a post-change latency delta is
not claimed yet.

### 2.5.17 September 19 response-materialization audit

The current artifact was re-run from an empty RustFS namespace after the
selected-object delta-closure and complete-member-union changes. All 20 pushes,
both incremental fetches and repacks, the final clone, and native full fsck
passed. The two incremental fetches measured 29.4 s and 34.5 s with 24 and 27
origin operations, and transferred 40.5 MiB and 41.6 MiB from the store.
The result is correctness-preserving but not a latency win: the union path is
gated for large selected sets, while this frontier still needs the full
checkpoint visibility proof.

The request trace identifies the actual bottleneck. The response packs
installed into the client were only 656 KiB and 881 KiB; the roughly 40 MiB
store responses were the layered checkpoint/control reads. The checkpoint
objects were 39--41 MiB because they contain the authenticated ordinal
visibility dictionary and admission map for the full staged repository. The
reader then decodes that dictionary and materializes the selected response
pack locally. Therefore a lower request count alone cannot make this fetch
fast: one 40 MiB GET still costs more than several small control GETs, and the
post-read Git/index-pack work remains on the critical path.

The optimization order is now explicit:

1. Keep the current exact admission, authenticated haves, thin-pack repair,
   and fail-closed fallback unchanged. They are correctness boundaries, not
   tuning knobs.
2. Split the large visibility proof from the checkpoint footer into immutable
   authenticated per-ref/ordinal-shard sidecars. The checkpoint carries each
   sidecar's hash, size, generation, and source-catalog digest. A single-ref
   fetch reads only the selected sidecar; a multi-ref clone reads the required
   sidecars in parallel. A missing, stale, or hash-mismatched sidecar falls
   back to the full authenticated proof, never to an incomplete view.
3. Cache verified sidecars and decoded ordinal indexes locally by content hash.
   Cache hits must verify size and BLAKE3 before use, write atomically, and be
   discarded on any decode or catalog-binding failure. This reduces repeated
   fetch latency without changing the object-store authority.
4. Reuse a verified generated response pack when the requested wants/haves,
   source catalog, and thin-base set are identical; otherwise use the current
   selected-member union and bounded writer. Cache identity must include all
   those inputs, so reuse cannot expose another ref or an older root.

The first item that should be implemented is sidecar visibility, not another
request coalescer. It is expected to remove tens of MiB of control transfer
and most visibility decode work while adding at most one authenticated sidecar
read for a single-ref fetch. The generated-pack cache is a secondary CPU/I/O
optimization; it must not be used as an integrity or authorization proof.

This audit refines the earlier observation that the 20-second fetch was mostly
pack materialization and read amplification: on the current full Kubernetes
frontier, the dominant amplification is the full visibility-control payload
plus local response construction, while the wire response pack itself is
small. The release gate therefore measures control bytes, source-range bytes,
response-pack CPU, and local validation CPU separately from raw request count.

### 2.5.18 September 20 stateless upload-pack control-read fix

The previous audit covered the legacy remote-helper fetch path, but ordinary
Git protocol-v2 fetches enter the stateless upload-pack path. That path was
still opening the complete layered visibility proof before it had seen the
fetch request. On the same populated Kubernetes-derived RustFS repository, a
20-commit fetch therefore used 12 object-store requests but returned
157,529,633 bytes in 23.111 s, including a full 40,561,173-byte checkpoint
GET.

The stateless path now opens the authenticated checkpoint footer and run
controls first, then uses an exact advertised-tip proof for ordinary,
unfiltered, non-shallow fetches. When the run controls contain a complete
authenticated old-tip-to-new-tip transition, the planner uses its exact
closure delta; otherwise it walks the complete reachable closure from the
authenticated advertised tips. It switches to the complete visibility proof
before planning any request that includes shallow/deepen state, a filter,
include-tags, or a policy that does not permit tip-only wants. This is a
protocol selection optimization, not a weaker authorization mode: every
shortcut is rooted in the authenticated advertised tips and rejects every
non-tip want.

The same fetch after the change used the same 12 requests, but returned
116,970,674 bytes in 13.786 s. The checkpoint read became a 2,214-byte range
(`bytes=40558959-40561172`); the remaining bytes were the selected capsule
pack ranges. The fetched remote tip was
`1124a801ebcedde8880b3cb9a4721745bad55c4c`, and local
`git fsck --connectivity-only` passed. This isolates the current bottleneck:
request count was flat, while control-byte transfer and local pack planning /
materialization dominated the avoidable portion of latency.

The release gate consequently treats these as separate budgets:

1. Ordinary warm fetches MUST avoid full layered-checkpoint-body reads.
2. Advanced fetches MUST retain the complete visibility-proof path and fail
   closed if the footer-only proof is not applicable.
3. Object-store request count alone is not a performance gate; checkpoint and
   source-range bytes, response-pack generation time, and client validation
   time are measured independently.

### 2.5.19 September 20 authenticated transition fast path

The footer-only reader already fetches the visibility-delta controls for the
active capsule frontier. Ordinary incremental fetches now reuse those exact
per-ref deltas when the request's advertised want is connected to one client
have by a complete old-tip-to-new-tip chain. The planner computes the final
closure difference from the authenticated `added`/`removed` sets, so it does
not re-walk unchanged trees or re-read the old commit closure. The response
still goes through the normal pack-entry CRC, delta-base, Git checksum, and
client `index-pack` validation paths.

This is strictly an optimization: a missing transition, a non-tip have, a
replacement ref, ambiguous history, shallow/deepen state, a filter, or tag
expansion falls back to the existing tip-bound traversal or complete proof.
The transition table is never an authorization shortcut; it is accepted only
after the capsule transaction and visibility edit agree on ref, old tip, and
new tip, and only for a chain rooted at a client-advertised have.

The expected effect is lower `visibility_plan_ms`, fewer source-range reads
for unchanged trees, and less local pack materialization work while leaving
object-store request count unchanged. Qualification must report plan time,
source bytes, pack-generation time, and final Git connectivity separately;
the transition path is not considered a pass until final-tip equality and
full fsck remain green on the 5,000-commit replay.

### 2.5.20 Authenticated layered-member fetch install

The next fetch optimization is now wired into the remote-helper path. After
the transition planner produces `wants - authenticated haves`, the reader
joins those object IDs with the frontier/run member-admission map. It takes
the direct path only when every requested object maps to authenticated
layered members, the fetched member indexes contain no object outside the
requested delta or proven haves, and no member declares an external
`REF_DELTA` base. The reader then range-reads only the selected sidecars and
pack bodies and atomically installs the immutable `.pack`, `.idx`, and `.rev`
files; it does not materialize a negotiated response pack or invoke local
`index-pack`.

The direct path is deliberately fail-closed. Missing admission, duplicate or
conflicting member identity, an unproven object, an external base, a shallow
or filtered request, or any sidecar/range integrity failure uses the existing
verified response-pack path (or returns the underlying corruption error).
Stable local pack bodies are skipped, and source ranges are coalesced within
the existing byte-amplification ceiling. This makes the common warm
incremental fetch a few immutable range reads plus sidecar validation while
preserving the old generated-pack path as the correctness fallback.
The remote helper holds the normal fetch-install fence through direct pack
installation, ref-tip validation, and the local connectivity walk, so the
multi-file fast path cannot race another fetch. It emits Git's
`connectivity-ok` directly after that proof instead of materializing a
duplicate full reachable response pack; the generated response path retains
the proof pack when direct member admission is unavailable.

### 2.5.21 September 20 local materialization reduction

The direct installer now carries the authenticated layered pack identity into
the sidecar-preserving local install. The pack body range is already checked
against its descriptor BLAKE3, and the authenticated index is checked against
the declared Git checksum and object count; repeating a complete SHA-1/BLAKE3
scan during installation only duplicated CPU and memory bandwidth. The
installer still validates sidecar bounds, index/reverse-index structure,
checksum agreement, atomic destination creation, and every required object
before exposing the pack to Git. A local pack that was already complete keeps
the stricter existing revalidation path.

The layered repository reader also limits its preferred index set to the
authenticated OID-admitted members. It no longer treats every visible stable
member as a preferred probe merely because one admission entry exists; an
unadmitted lookup still falls back to the complete inventory. This reduces
index GET/probe fan-out without changing the authorization or corruption
fallback contract. The read crate suite remains green (199 tests), and the
remote-Git suite remains green (138 tests). The current full CLI now builds;
the fresh 5,000-commit Kubernetes latency gate remains open.

### 2.5.22 September 20 direct-fetch connectivity reduction

The direct layered-member path no longer creates a second connectivity-proof
pack after installing authenticated members. It proves the requested ref tips
with `git rev-list` over the exact planned common-have frontier, validates that
the walk is complete and has no missing objects, and emits `connectivity-ok`.
This preserves the Git remote-helper contract while removing a full reachable
`git pack-objects` pass over the local repository. If member admission is
ambiguous, a sidecar has an external delta, or the connectivity walk fails,
the path remains fail-closed and uses the existing generated response pack.

The current `crab 1.2.4` binary (SHA-256
`1a34502e69736ec7136e05854ef88a6326e124360cd332009fceb594fb7613aa`) was
exercised against an isolated local RustFS fixture with
20 pushes, fetches and suffix repacks at pushes 10 and 20, a final clone, and
native full fsck. Both incremental fetches completed in 370 ms and 435 ms,
with 35 and 32 origin operations; the final tip matched and full fsck passed.
The 20 incremental pushes averaged 509 ms and 9.8 origin operations (p50
429 ms, p95 782 ms). This is a small-repository smoke, not Kubernetes or
5,000-commit qualification, but it confirms the direct path and connectivity
contract are end-to-end reachable after the materialization change.

### 2.5.23 September 20 connectivity-walk overhead reduction

The direct path keeps the same complete connectivity proof, but removes two
avoidable local costs. Ref-tip and common-have existence checks now share one
`git cat-file --batch-check` process, and the proof walk invokes
`git rev-list --no-object-names` because path names are not part of the
connectivity contract. The walk still enumerates every reachable object and
still reports missing objects; this changes only process startup and pipe
volume, not the accepted object set or fail-closed behavior.

The focused connectivity suite passes all 13 tests and the remote-helper suite
passes all 139 tests after this change. A large-repository latency number is
intentionally not inferred from the small RustFS smoke; the fresh Kubernetes
5,000-commit qualification remains the release gate.

### 2.5.24 September 20 current-binary protocol smoke

The rebuilt binary (`crab 1.2.4`, SHA-256
`067330d9cf73d77b2965229bc4ee0d620bea25b379d9e1991ce901e26dd9144e`) passed
the full RustFS protocol-v2 partial-clone smoke (`incremental-fetch-opt-20260920`).
The real filtered incremental fetch completed in 647 ms, transferred 33,221
bytes across 12 origin operations, and the report finished with `status=passed`.
The run also passed full/filtered/shallow clones, lazy fetches, pointer and
security checks, strict fsck, and the protocol disconnect checks. It is
supplementary fixture evidence; it is not a substitute for the large
Kubernetes replay or the warm 500-commit fetch gate.

### 2.5.25 September 20 fresh layered Kubernetes-fixture replay

The same rebuilt binary was replayed from an isolated, read-only
Kubernetes-derived fixture with 20 first-parent commits. The run used local
RustFS, seeded the remote, created an incremental clone, fetched and repacked
at pushes 10 and 20, created a final clone, checked the final tip, and ran
native full fsck. The report is
`/Users/haipingfu/Workspace/CrabBuild/layered-fixture-e2e-20260920-2/artifacts/report.json`
and records `status=passed` with the binary SHA-256 above.

| Stage | Time | Origin operations |
| --- | ---: | ---: |
| Seed push | 1.983 s | 11 |
| Incremental push mean / p50 / p95 / max | 348 / 327 / 398 / 560 ms | mean 9.8 (p50 8, p95/p99 13) |
| Incremental fetch at 10 | 1.336 s | 32 |
| Incremental fetch at 20 | 418 ms | 32 |
| Final lazy clone | 2.931 s | 311 |

All 20 incremental pushes completed below the ten-operation mean target, and
both fetches preserved the expected tip and full-fsck result. The 32-operation
fetch includes the fixture's clone/fetch admission and validation work; it is
not evidence that the 500-commit warm-fetch budget is met. The full 1.2-GiB
Kubernetes seed and the fresh 5,000-commit replay remain mandatory release
gates.

### 2.5.26 September 20 incremental-fetch CPU proof reduction

The fetch path now removes the last duplicate full-pack materialization from
incremental responses. Direct layered-member admission and the generated
incremental fallback both validate ref tips and the authenticated common-have
frontier with one `git cat-file --batch-check` process, then run the complete
streaming `git rev-list --objects --no-object-names --missing=print` walk. The
legacy complete-checkpoint path retains its proof-pack behavior. Thus the
optimization changes neither the authenticated object set nor the fail-closed
connectivity contract; it removes only the extra `git pack-objects` pass and
the path-name bytes that were never part of the proof.

The patched tree passed `cargo check -p crab --lib --no-default-features
--locked`, all 139 remote-helper tests, formatting, and `git diff --check`.
The post-patch large-repository binary could not be linked in this workspace
because the qualification volume exhausted its remaining capacity during the
link step. The in-flight 5,000-commit run uses the preceding binary and is
therefore not evidence for this change; its report remains an open gate. A
new RustFS measurement is required before claiming a large-repository latency
improvement. Object-store request counts are expected to stay flat; the target
is lower local pack-generation, SHA-1, and index-pack CPU on the critical path.

### 2.5.27 September 20 authenticated connectivity proof for incremental fetch

After authenticated layered-member admission, the client has exact sidecar
coverage for every planned delta object and every retained common-have. A
direct layered-member install therefore returns `connectivity-ok` from that
proof after validating the requested tips; it does not run a second
`git rev-list` walk over the same large trees and blobs. The proof is
fail-closed: every required object must be covered, every selected member must
be self-contained, and no selected member may contain an object outside the
authenticated delta/common-have set. Generated response-pack installs retain
the `git rev-list --quiet --missing=print` walk, which suppresses serialization
and parsing of present OIDs while still reporting any missing object.

This is not an authorization shortcut. The direct path reaches it only after
authenticated member admission, pack/index identity validation, and local
ref-tip validation. The focused connectivity suite passes all 13 tests
(including missing-object detection); `cargo check` passes with the existing
workspace warnings. The direct proof has no object-store request impact; it
targets the large local graph walk. A full Kubernetes latency delta remains
unclaimed until the disk-capacity issue is resolved and the 5,000-commit gate
is rerun with the rebuilt binary.

The current-binary Kubernetes qualification reached seed push (1.16 GiB, 12
requests) and seed repack (148 s, 15 requests), then was stopped during the
fresh clone when the qualification volume reached 100%; it is recorded as a
non-qualifying capacity failure, not a protocol result.

### 2.5.28 September 20 rebuilt-binary bounded smoke after quiet proof

The rebuilt binary (`crab 1.2.4`, SHA-256
`139ce7b0835170d231ea103e6f54c5eca2060ef940a5a813e9c50a2f69dc87b6`) passed
the 20-commit local RustFS fixture with seed push, two incremental fetches,
two suffix repacks, incremental and final lazy clones, tip equality, and full
fsck. Pushes averaged 174 ms with 9.8 object-store operations; the two
incremental fetches completed in 230 ms and 249 ms. Their request counts were
32 and 35 and response bytes were 150 KiB and 165 KiB, so the request target
is not met by this small fixture, but the fetch is now bounded by a few small
pack/control ranges rather than a whole-repository pack. This is bounded
evidence only; it does not replace the blocked 5,000-commit gate.

### 2.5.29 September 20 zero-copy layered range materialization

Authenticated layered sidecar and pack ranges are now retained as `Bytes`
sub-slices of their verified source windows until the atomic Git install
finishes. The previous path copied every range once after hashing it and then
copied it again into the temporary install file. The new path removes that
intermediate allocation without changing the hash check, sidecar validation,
pack identity validation, or atomic publication boundary. This is a local
memory-bandwidth optimization: object-store requests and range boundaries are
unchanged, and the source window remains alive for the entire install call.

### 2.5.30 September 20 post-optimization RustFS smoke

The post-change binary (`crab 1.2.4`, SHA-256
`0dff8ebef4c9a08539afb8b6d5b5d83752c60844b819e2f17a7326683d43c57e`) was
replayed against the same 20-commit Kubernetes-derived RustFS fixture. Seed
publication, both incremental fetches, both suffix repacks, the final clone,
tip equality, and full fsck all passed. Fetches completed in 251 ms and 245 ms
with 32 origin requests each; pushes averaged 187 ms and 9.8 requests. The
small fixture shows no statistically meaningful latency delta versus the prior
bounded run, which is expected because its local ranges are already small;
the change is aimed at large layered pack windows. The 5,000-commit gate is
still open and no large-repository speedup is claimed from this smoke.

### 2.5.31 September 20 warm local-pack validation optimization

The direct fetch installer now treats an already-installed, content-addressed
member as a local admission candidate. During each admission it verifies the
pack BLAKE3, index and reverse-index BLAKE3 values, Git pack checksum, object
count, and object-ID coverage, then skips the installer’s second full SHA-1
scan and does not range-read locator metadata that ordinary unfiltered fetches
do not consume.
New members retain the complete authenticated sidecar/pack path. No cache hit,
ETag, or filename is used as integrity proof; the one local BLAKE3 scan and
index validation remain mandatory. This reduces local read amplification while
preserving the corruption and race failure behavior.

The planner also uses the authenticated per-ref transition chain when the
client's have reaches the requested tip. It computes the exact added-object
set from transition deltas and skips repository-wide object reads; if the
chain is missing, ambiguous, shallow, or otherwise incomplete, it falls back
to the bounded visibility walk. This keeps the fast path correctness-first
while removing the dominant `read_many`/visibility cost from ordinary warm
incremental fetches.

The rebuilt local-fast binary passed the same 20-commit RustFS replay with
20 pushes, two fetches, two suffix repacks, final clone, tip equality, and
full fsck. Fetches were 275--301 ms with 32 origin operations and 150--165
KiB of response bytes; push mean was 285.55 ms at 9.8 operations. The selected
members in this small fixture were mostly new, so the request count and wall
time are not a statistically significant improvement over the earlier
252--259 ms/14-operation bounded result. The result is a non-regression and
correctness proof; large-pack latency remains an open qualification gate.

### 2.5.32 September 20 cold-clone fast path and large-range streaming

The one-member layered cold-clone path is now selected before capability
advertisement when the destination Git object database is empty. The helper
omits `stateless-connect` for that narrow, authenticated case, so Git uses its
classic `fetch` contract. Crab installs the source pack, index, and reverse
index directly and returns `connectivity-ok`; Git does not receive the pack on
stdout and therefore does not run a second `index-pack` over an already indexed
pack. Existing repositories continue to advertise protocol v2, preserving
filter, shallow, and negotiated incremental-fetch behavior.

The direct installer is fail-closed: it admits only one layered source/member,
rejects capsules and external delta bases, range-reads and hashes the exact
pack body, hashes every sidecar, validates the Git checksum/object count and
locator, then atomically publishes the immutable `<pack,idx,rev>` tuple. Large
pack bodies retain the bounded 128 MiB/10-request fallback, while the
S3-compatible signed path uses 512 MiB ranges with at most four requests in
flight and writes each response at its authenticated local offset. When the
pack and its three sidecars are contiguous, the sidecar span is coalesced into
the final pack range and sliced locally. Neither path assembles a second
1.27 GiB buffer, and small ranges plus all warm/incremental paths retain their
existing request shape.

For a cold layered clone, connectivity is verified with Git's native
`fsck --connectivity-only --no-dangling` walk after the authenticated pack is
installed. It still checks every reachable commit, tree, and blob reference,
but avoids emitting a textual OID stream; the detailed `rev-list` checker
remains in place for incremental and frontier fetches.

On the local RustFS Kubernetes-derived fixture (one 1,271,689,123-byte Git
pack), the final rebuilt binary produced a clean cold clone in 70.79 s with
`--checkout` and 65.27 s with `--no-checkout`; native full `git fsck` passed,
the expected tip was present, and the installed pack/index/reverse-index were
valid. The earlier protocol-v2 direct-stream result was 131.52 s end-to-end.
The range path reduced the single-object wire ceiling, but RustFS still logged
6--16 s decode times for individual 128 MiB ranges, and local checkout plus
pack/index validation remain material. This is a measured regression reduction,
not a claim that a 1.27 GiB cold clone can finish in a few seconds. A few-second
large clone requires a faster object-store read path and/or pre-installed
client-side pack/index distribution; the 5,000-commit qualification gate stays
open.

The final rebuilt binary later completed a no-checkout clone in 109.75 s on the
same fixture, with the expected tip/tree and a zero-exit connectivity-only fsck
in 5.18 s. The spread across runs is endpoint-dependent: direct RustFS range
benchmarks for this object varied from roughly 7--15 s, while the helper run
also includes authenticated sidecar reads, pack hashing, Git's local
post-fetch connectivity pass, and filesystem scheduling.

### 2.5.33 September 20 signed-range coalescing re-audit

The signed cold-clone reader now coalesces a sidecar range that begins exactly
at the pack end into the same bounded range schedule. It downloads the
combined span into the temporary file, reads the sidecars back from their
authenticated offsets, truncates the file to the pack boundary, and then
hashes the installed pack. This removes one body request without weakening
the proof: every returned sidecar still matches its descriptor, the pack still
matches its BLAKE3 identity, the indexes still agree with the Git checksum and
object count, and the locator still validates the pack kind metadata. A
non-contiguous layout keeps the four-request shape (three 512 MiB pack ranges
plus one sidecar range), and a provider that cannot return `206` falls back to
the existing object-store range implementation.

The release binary was requalified against the existing Kubernetes-derived
RustFS fixture. Before coalescing, the 1,271,689,143-byte pack and
54,578,444-byte sidecars took 36.3 s inside the signed-range reader and 55.2 s
end-to-end; the expected tip was installed and the connectivity walk passed.
After coalescing, the same clone took 50.0 s in the signed reader and 65.4 s
end-to-end; a subsequent full `git fsck` passed and the expected tip remained
unchanged. These timings are not a valid throughput verdict: RustFS was
consuming roughly one host CPU while unrelated virtualization and Rust builds
were active. They do prove the optimized path is reachable, bounded, and
byte-correct. The few-second cold-clone target remains an open gate requiring
an idle-host release run (and, if that run is still slow, a faster local
RustFS/client transport), not a relaxed integrity check.

### 2.5.34 September 20 current-binary cold-clone requalification

The previous direct-clone failures were not pack-transfer failures. The
installed `git-remote-crab` was a Homebrew 1.0.1 helper, while the tested
release binary was 1.2.4; that helper selected the AWS SDK store for a custom
RustFS endpoint, treated the v2 root as a v1 layout, and stopped before any
pack bytes were read. Store selection now keeps custom S3-compatible endpoints
on the native adapter, and the auth test covers both the AWS-default and
custom-endpoint decisions.

With an isolated helper symlink to the rebuilt 1.2.4 binary, the same fixture
completed a no-checkout cold clone in 29.06 s. The signed coalesced range read
transferred the 1,271,689,143-byte pack and 54,578,444-byte sidecars in
16.04 s using three bounded range requests; Git's native connectivity walk
completed before the helper returned, the expected tip was
`439cad0dea409498dd607d9c35e9e0353c15667e`, and a subsequent full
`git fsck --full --no-reflogs --no-progress` passed. The run was made while
the Docker VM and RustFS were under active load, so it is a correctness and
regression datapoint, not an idle-host throughput claim. A 1.4 GiB cold clone
cannot be promised in a few seconds without a faster RustFS storage path or a
warm local pack cache; the idle-host few-second gate and the 5,000-commit
qualification remain open.

### 2.5.35 September 20 authenticated cold-clone connectivity fast path

The one-source/one-member cold-clone path now consumes the layered ordinal
visibility admission proof after the pack, indexes, reverse index, locator, and
advertised tips have been validated. When every visibility ordinal maps to the
installed member, the proof establishes that the complete visible object
universe is present; Crab returns `connectivity-ok` without launching a second
full Git reachability walk. This removes redundant local graph traversal while
preserving the fail-closed boundary: checkpoints without the complete proof,
legacy snapshots, multi-source views, or any ambiguous admission still use
native `git fsck --connectivity-only`. Strict `crab fsck` and native full fsck
remain available as independent verification gates.

The focused `crab-read` suite remains green (200 tests). A release cold-clone
rerun on an idle host is still required to quantify the latency reduction; the
earlier 29--70 s runs were host-loaded datapoints, and the 1.27 GiB transfer
itself measured about 16 s even before local verification. The few-second gate
therefore remains open until both the storage transfer and Git's local clone
work are measured on an idle RustFS host.

### 2.5.36 September 21 single-pass cold view selection

The remote helper now decides whether the client is fresh before loading the
capsule view. A fresh layered clone opens the complete authenticated checkpoint
once, while a fetch with local haves opens only the footer/control view. A
cached control view is promoted from its pinned root only when the client is
fresh, so the cold path no longer reads the same checkpoint control twice and
the warm path cannot accidentally reopen stable pack metadata or bodies.
This is a request and latency reduction only; pack hashes, sidecar hashes,
Git identities, visibility admission, ref-tip validation, and the fail-closed
connectivity fallback are unchanged.

### 2.5.37 September 21 bounded cold-clone admission proof

The direct layered cold-clone path now loads the bounded checkpoint body when
the normal view contains only its authenticated footer. It verifies the
ordinal visibility dictionary, requires every ordinal and advertised ref tip
to admit to the one installed source/member, and retains the existing pack,
index, reverse-index, locator, BLAKE3, Git-checksum, and object-count checks.
Only after that proof succeeds does the helper skip the redundant
`git cat-file` ref-tip walk; incomplete, legacy, multi-member, or ambiguous
views still run the native validation path. This removes local graph-read
amplification without changing object-store range boundaries or weakening the
fail-closed contract. The proof is bounded by the checkpoint limit and never
loads the capsule/run body.

### 2.5.38 September 21 fresh uncheckpointed-root cold clone

The fresh-root path is now covered as well as the checkpointed path. A root
with no layered checkpoint may use the direct installer only when its
authenticated frontier is exactly one run and one self-contained member. The
run admission must cover every indexed object, every advertised and peeled ref
tip must be admitted to that member, and any external `REF_DELTA` base keeps
the native connectivity proof. An uncheckpointed multi-run root is promoted
to the complete reader before installation, so the control-only optimization
cannot omit an older run.

On a fresh local RustFS instance, a Kubernetes-derived fixture containing one
1,271,689,143-byte Git pack published its initial root in 333.41 s (the
fixture's 1,443,695,618-byte capsule was the dominant upload). A fresh
uncheckpointed clone completed in 46.95 s, installed one authenticated
`pack/.idx/.rev` tuple, and reached the expected tip. A separate native
`git fsck --full` passed in 113.22 s. The object store contained five
immutable objects for the root, ref, lock, capsule payload, and metadata; the
clone did not fall back to materializing the complete capsule. The measured
pack transfer is roughly 27 MiB/s, so a few-second 1.27 GiB clone is not a
realistic local-RustFS claim without a substantially faster storage/HTTP path
or a warm client-side pack cache. This is a regression fix and correctness
datapoint, not a 5,000-commit qualification result.

### 2.5.39 September 21 large authenticated-range fast path

The remaining cold-clone stall was in the control proof, not in Git pack
transfer. Large `Store::range_get` calls now use the provider's presigned
streaming transport when the store has a signer and no scoped read routes. The
response must still be an exact `206` range of the requested length; the
existing object-store range implementation remains the fail-closed fallback
when presigning or range responses are unsupported. Read observers and retry
classification remain on the signed path, and all callers continue to verify
the range's committed hash or structure before using it.

On a fresh local RustFS repository containing the same Kubernetes-derived
1,271,689,143-byte pack, the rebuilt release binary completed the signed pack
and sidecar transfer in 4.68 s and a no-checkout cold clone in 12.46 s. The
clone installed one authenticated `pack/.idx/.rev` tuple, did not spawn
Git's `index-pack`, reached `439cad0dea409498dd607d9c35e9e0353c15667e`, and
passed independent connectivity-only fsck in 5.31 s and full fsck in 107.86 s.
Materializing the working tree afterward took 11.51 s on the same host, so a
default checkout is expected to add that local filesystem cost. This removes
the measured ~37 s helper-side control-range stall and is a material
improvement over the previous 45--47 s no-checkout datapoint; it does not
claim a few-second full working-tree clone or close the 5,000-commit gate.

### 2.5.40 September 21 cold-clone sidecar validation re-audit

The direct installer previously validated the authenticated index and reverse
index, then reopened and walked the same pair a second time while staging the
pack. The installer now has an explicit verified-sidecar entry point. It still
enforces the pack-size and sidecar-size bounds, content identity, Git checksum,
object count, locator metadata, and atomic `<pack,idx,rev>` publication; it only
reuses the proof for the exact temporary sidecar paths that were just checked.
No integrity check is removed, and the normal unverified installation API keeps
its original validation behavior.

A fresh local-RustFS run from the patched release binary used the same
1,271,689,143-byte pack and 54,578,444-byte sidecar span. Signed range transfer
took 4.99 s, a fresh `git clone --no-checkout` took 13.04 s, and the actual
checkout added 5.85 s. The expected tip was
`439cad0dea409498dd607d9c35e9e0353c15667e`; connectivity-only fsck passed in
5.81 s. The trace showed no `git index-pack`; the remaining no-checkout time is
the local Git post-fetch `rev-list` walk plus pack/index installation, not an
object-store request stall. This closes the 50-minute fallback regression, but
the few-second full-clone target remains open for a 1.27 GiB repository and
requires a client-side commit-graph/pack-cache or faster local filesystem path.

### 2.5.41 September 21 authenticated object-set proof

The direct cold-clone path no longer loads the full ordinal visibility body
just to decide whether it may skip Git's connectivity walk. Every checkpoint
written with `build_with_ordinal_visibility` now commits a domain-separated
digest of its canonical sorted object dictionary in the authenticated footer.
The control-only reader can therefore decide eligibility from bounded metadata:
one source, one member, no external delta bases, and a present object-set
digest. After the single coalesced pack/sidecar read, the installer hashes the
sorted pack-index object IDs, compares that value with the footer digest, and
checks every advertised ref tip is present before returning `connectivity-ok`.
The footer also commits the visibility object count. If the authenticated pack
has extra objects, its count differs and the helper keeps the normal native
connectivity walk; it never treats a visibility digest as proof for a partial
or ambiguous pack.

This is an optimization of duplicate work, not an authorization shortcut. A
missing digest (including every pre-digest checkpoint), a malformed index, a
digest mismatch, a missing ref tip, multiple members, or an external delta base
fails closed to the existing native validation path or returns corruption; no
object is admitted from a probabilistic hint. The focused metadata tests cover
digest round-trip/control decoding; the read suite covers the surrounding
control-view and sidecar-integrity paths, while the full installer comparison
remains an end-to-end qualification gate.
The fresh local-RustFS 1.27 GiB datapoint remains 4.99 s for the signed range,
13.04 s for `git clone --no-checkout`, and 5.85 s for checkout; the next
qualification must rerun that fixture with this proof and an idle host before
calling the few-second target closed.

### 2.5.42 September 21 cold-clone transfer and connectivity tuning

The contiguous pack-plus-sidecar range is now downloaded with one sequential
signed GET. The stream hashes the pack prefix as it is written, so the reader
does not reread the full pack from local disk solely to verify its BLAKE3
identity. Non-contiguous ranges retain the bounded concurrent path. A complete
object-set proof also returns `connectivity-ok` without asking Git to repeat a
whole-repository connectivity walk; the helper never does this for an old,
missing, multi-member, external-delta, count-mismatched, or otherwise
unproven admission.

This reduces duplicate local I/O and Git graph work without changing the
authenticated range, pack identity, sidecar validation, index identity, ref-tip
presence, or atomic pack publication checks. Qualification must still measure
the backend's sequential range throughput separately: a local RustFS disk can
make the same one-request transfer vary materially even after request count is
flat.

### 2.5.43 September 21 cold-clone qualification after Git lock handoff

The authenticated one-pack path now returns Git's standard `.keep` marker for
the exact pack whose index and object-set digest were just verified. Git can
associate `connectivity-ok` with that pack and closes its post-fetch
`rev-list` with an empty input. The marker is created exclusively and is
removed by Git at the end of the fetch; if the pack is not exactly one
verified `.pack`, Crab does not emit the marker and Git retains its normal
connectivity walk.

On the fresh local-RustFS Kubernetes fixture (1,123,511,228-byte Git pack,
1,488,508 admitted objects), the no-checkout clone completed in 20.09 s. The
single coalesced signed range took 18.07 s, while Git's connectivity process
took 2.7 ms and no `.keep` file remained afterward. A direct RustFS GET to
`/dev/null` took 5.60 s; writing the same object to the qualification volume
took 19.21 s. The remaining latency is therefore destination-volume write
throughput (RustFS data and the clone destination share that volume), not
object-store request count or a graph-validation regression.
The few-second target requires a destination filesystem that can sustain the
pack write rate (or a smaller/filtered clone); a full 1.1-GiB Git pack cannot
be made a few seconds on a ~60--70 MiB/s destination without changing the
bytes materialized.

### 2.5.44 September 21 ref-only control regression and fast-destination requalification

The first control-only layered-view implementation validated a physical pack
descriptor for every run. A ref-only run intentionally has no Git members, so
that validation rejected an otherwise valid view as a corrupt pack before the
fetch could select its authenticated ref state. The reader now validates the
source descriptor only for runs that actually carry Git members; ref-only runs
still pass through the authenticated run, visibility, and transition checks.
This keeps the physical-source invariant strict without treating a metadata
run as a malformed pack. The focused selector tests pass 3/3, the complete
`crab-read` library suite passes 200/200, and all 139 remote-helper tests pass.

The exact patched release was then exercised against the same local RustFS
Kubernetes-derived fixture. A cold `git clone --no-checkout` of the 1.1-GiB
pack completed in 3.75 s on a separate fast local destination, reached
`71f0fc6e72d53d5caf50b1314ca4d754463117f0`, installed three pack sidecar
files, and passed connectivity-only fsck in 8.12 s. A default clone including
the 26,892-file checkout completed in 8.04 s and reached the same tip. The
earlier 20.09 s result used a destination sharing the qualification volume
with RustFS; its 19.21 s pack write, versus 5.60 s to `/dev/null`, remains a
filesystem-throughput datapoint rather than a protocol regression.

This closes the ref-only control regression and demonstrates the few-second
cold-clone target when the destination is not contending with RustFS. The
5,000-commit replay, hosted-provider latency, and shared-volume throughput
gates remain separate release qualifications; v1 must not be retired until
those gates meet the matrix in section 13.2.

### 2.5.45 September 21 canonical ordinal and multi-pack cold-clone re-audit

The first fresh PR-208 replay after the ref-only fix exposed a second
correctness boundary: ref updates append unseen object IDs to the in-memory
visibility dictionary, but the layered wire format requires a strictly sorted
OID dictionary. The writer now canonicalizes that dictionary at the wire
boundary and remaps refs, transitions, history closures, and authenticated
member admission through one old-to-new ordinal map. This preserves the cheap
append-only runtime representation while making the serialized proof
canonical. A focused regression test exercises an unsorted three-object
dictionary and verifies that the authenticated member order follows the
canonical remap. The full `crab-metadata` library suite passes 212/212.

The next local-RustFS smoke (seed plus ten replay pushes, with fetch and
suffix-repack checkpoints at five and ten) passed every push, fetch, repack,
tip, and fsck gate. After the seed, incremental pushes remained approximately
1.04 s each. The same run also showed the remaining cold-clone blocker: the
checkpoint contained multiple active pack members, so the one-source/
one-member direct installer correctly declined admission and the normal
upload-pack path performed 357,313 uncoalesced range reads in 163 s before the
qualification was intentionally stopped. It had read 1.89 GB and inflated
15.2 GB while producing only about 80 KiB of destination output. This is a
request-amplification and pack-materialization failure, not a correctness
pass; the multi-member direct-install path (or an equivalent authenticated
whole-member union path) is still required before the cold-clone gate can
close.

Accordingly, this re-audit claims only the ordinal correctness fix and the
ten-push incremental smoke. It does not claim a passing 5,000-commit replay,
multi-pack cold clone, or v1 retirement.

## 3. Goals

The implementation MUST:

1. Preserve every existing v2 publication, authorization, snapshot, Git-object,
   xorb, shard, and GC correctness invariant.
2. Give each immutable Git pack body an identity independent of its containing
   capsule or layer and the checkpoint generations that reference it.
3. Represent one complete repository view as an authenticated ordered pack set
   plus a bounded post-checkpoint capsule frontier.
4. Reuse the stable pack prefix across checkpoints without reading or writing
   its bodies.
5. Repack only a geometrically selected suffix and atomically replace that
   suffix in the next checkpoint.
6. Make incremental fetch transfer wants minus proven common haves and avoid
   stable-source body reads.
7. Support fresh clone, partial clone, shallow fetch, lazy fetch, browser reads,
   mount reads, and direct remote-helper installation from the same pinned pack
   set.
8. Keep canonical xorb and shard payloads outside Git pack layers and
   checkpoints.
9. Trace all pack sources reachable from the current root, retained history,
   active readers, and grace-period snapshots before GC can delete them.
10. Keep the foreground small-push request budget unchanged.

## 4. Non-goals

This plan does not:

- make pack maintenance part of the clean push transaction;
- concatenate multiple packfiles on the Git wire;
- rely on Git `packfile-uris` for correctness;
- force a complete repository repack every 500 commits;
- put xorb or shard payloads in a pack layer;
- use a cache hit, ETag, Bloom filter, or local filename as integrity proof;
- promise constant cold-clone reads regardless of repository size and filter;
  or
- add a runtime fallback from the new v2 format to `CRBCKP03`.

## 5. Ownership and module shape

The design uses one deep pack-set module instead of spreading layer policy
across commands:

| Module | Responsibility |
| --- | --- |
| `crab-metadata::capsule_protocol` | Versioned pack-layer and checkpoint contracts, deterministic encoding, bounds, and hash validation |
| `crab-storage::StoreLayout` | Canonical pack-layer and checkpoint object paths |
| `crab-write::capsule_protocol` | Create-only layer publication followed by exact-base root CAS |
| `crab-read::capsule_protocol` | Pinned pack-set open, locator resolution, selective range reads, direct local installation, and integrity checks |
| `crab-git::repack` | Pure geometric selection and verified suffix consolidation |
| `crab` commands | Scheduling, observability, operator policy, fsck, GC, and qualification |

The narrow internal interface should expose operations equivalent to:

- open and authenticate one pack set;
- resolve an object and its delta-base closure;
- install eligible missing pack sources into a local Git object database;
- build one checkpoint from stable sources plus frontier runs;
- select and replace a geometric suffix; and
- enumerate the immutable objects reachable from a checkpoint.

Callers must not reconstruct storage keys, reinterpret layer ordering, or
implement a second locator merge.

## 6. Storage format

### 6.1 Layout

```text
{repo}/v2/
├── root
├── refs/heads/{ref-key}.json
├── capsules/{first-two-hex}/{capsule-hash}
├── pack-layers/{first-two-hex}/{layer-hash}
├── checkpoints/{first-two-hex}/{checkpoint-hash}
└── history/{first-two-hex}/{history-hash}
```

Pack layers, checkpoints, capsules, and history segments are immutable and
create-only. The root and ref heads remain the only mutable publication
authorities.

### 6.2 Pack source and member

The source is the bounded physical read and maintenance unit. One
`PackSourceDescriptor` binds:

```text
source kind: capsule run or standalone layer
source object hash and size
source-control offset, size, and BLAKE3
ordered member count and aggregate object count
compressed-byte weight
source-local transition range
```

Its authenticated control suffix contains an ordered directory of
`PackMemberDescriptor` records:

```text
pack-body content hash, absolute range, and BLAKE3
contiguous sidecar range plus individual section BLAKE3 values
Git checksum and object count
index, reverse-index, and locator commitments
transaction/visibility identity for capsule members
declared external delta-base identities
```

This distinction is required for both correctness and request efficiency. A
500-push frontier may contain hundreds of tiny Git packs, but its binary
capsule-run inventory contains only a bounded number of physical objects. The
protocol therefore limits physical sources, not individual Git pack members.
Treating every capsule pack as one source would either exceed the source bound
or force a synchronous physical repack at every checkpoint.

The source kind determines the object path through `StoreLayout`; serialized
records never carry an arbitrary object-store key. A capsule-run source uses
`CRBRUN04`, which preserves the complete capsule bytes, adds one aggregate,
authenticated pack-member directory, carries the transaction plus small
visibility/catalog controls in the run footer, and appends an authenticated
sorted OID-to-member admission sidecar. A control section larger than
512 KiB remains committed by its capsule range/hash but is detached from the
footer; the bounded reader fetches that exact range, verifies its BLAKE3, and
then materializes the control. This keeps a production-sized initial
visibility proof from turning the run footer into a hot multi-megabyte object
without weakening authorization. A reader opens the authenticated suffix
(trailer discovery is currently a second small range read) and can then
address all nested `CRBCAPS2` pack bodies and sidecars without fetching the
capsule payload. A standalone source binds one member in a `CRBPKL01` object.
In both cases the reusable local Git pack filename is derived from the
pack-body content hash, not the containing source identity.

This indirection makes checkpoints request-minimal: newly checkpointed capsule
runs become stable pack-set sources without a payload copy. Their run objects
remain reachable through the checkpoint after the transaction frontier no
longer names them. Later checkpoints carry each old source descriptor forward;
they do not rebind old members to a newer run merely because frontier run
compaction copied the same capsule bytes.

### 6.3 Standalone pack layer

`CRBPKL01` is one immutable object with pack payload first and one authenticated
control suffix:

```text
Git pack bytes
Git index
Git reverse index
Git object locator, including external REF_DELTA base identities
layer footer
footer length
footer BLAKE3
CRBPKL01
```

The layer pointer commits to:

- whole-object BLAKE3 and size;
- Git pack checksum, byte range, and object count;
- control offset, size, and footer BLAKE3;
- index, reverse-index, and locator section ranges and BLAKE3 values; and
- the set of external delta-base object IDs, when non-empty.

The pack body remains range-addressable. Metadata readers load only the
control suffix. A complete layer download verifies the object BLAKE3, Git pack
trailer, indexes, locators, entry CRCs, reconstructed object IDs, and every
declared external base.

### 6.4 Checkpoint

`CRBCKP05` is metadata-only. It contains:

```text
covered generation and root digest
ordered pack-source descriptors
authenticated directory of source-local control commitments
compact ordinal visibility proof and transition evidence
pointer catalog for external shard/xorb dependencies
checkpoint footer
```

The visibility item is a source-catalog-bound ordinal proof. Its dictionary and
transition closures are authenticated against the ordered source catalog, so
the hot path does not transfer repeated 20-byte OIDs. The complete OID view
remains available to strict fsck, restore, and migration tooling, but is not
part of the CP05 warm-fetch control path.

The checkpoint hash authenticates the pack-set order and every source
descriptor. The root's checkpoint pointer authenticates the checkpoint hash,
size, covered generation, covered root digest, physical-source count,
pack-member count, and object count.

Pack-set order is oldest/largest to newest/smallest. Member directories and
locators remain with their immutable source rather than being recopied into
every checkpoint. A reader opens only the source controls selected by
transition provenance and builds a merged in-memory view for the lifetime of
its pinned operation. Duplicate object IDs across sources are permitted only
when publication proves byte-identical Git objects; the newest locator wins
deterministically. A different object for the same Git ID is corruption.

The active physical-source count has a protocol hard limit of 64; each capsule
run has the existing 512-capsule bound. Successful steady-state maintenance
targets at most eight active physical sources so a cold control open fits in
one small parallel wave. The hard limit protects correctness during a
maintenance backlog; it is not the performance target. Crossing it requires a
bounded suffix roll-up before another source can become visible, but never a
whole-repository foreground repack.

The checkpoint remains a complete repository read view even though its Git
bytes live in multiple immutable objects. Xorb and shard catalogs remain
authenticated metadata references to their existing external objects.

### 6.5 Delta dependencies

A pack source may contain `REF_DELTA` entries whose bases live in an earlier
retained source. The locator names each base object ID, and publication proves
the full dependency closure against the candidate pack set before root CAS.

The following are forbidden:

- `OFS_DELTA` references across layer objects;
- a base that exists only in a later layer;
- a base reachable only from an uncommitted capsule;
- a dependency cycle; and
- removing a source object while a retained source still depends on it.

Suffix consolidation may use stable-prefix objects as temporary verified
delta bases, but its replacement layer must declare those external base IDs.
GC derives capsule and layer retention from both checkpoint membership and
dependency closure.

## 7. Publication and checkpoint protocol

### 7.1 Foreground push

Foreground push is unchanged:

1. pin the root/ref authority and expected old tip;
2. validate and prepare one capsule;
3. make every new Git and external large-file dependency durable;
4. create the immutable capsule; and
5. commit with the per-ref or multi-ref authority CAS and confirm the root
   epoch.

It does not read, rewrite, or publish stable pack layers.

### 7.2 Checkpoint without geometric collision

Maintenance pins one complete view and carries its stable descriptors plus the
eligible capsule-run source descriptors into the candidate pack set. It then:

1. validates every new capsule-run source and its member dependency closure
   against the pinned pack set;
2. builds a metadata-only checkpoint containing the old stable sources plus
   the newly stable capsule-run sources;
3. verifies refs, visibility, pointer catalog, locator coverage, and declared
   delta dependencies using the publication-time proofs already bound to each
   immutable source;
4. uploads the checkpoint and history segment; and
5. publishes them with an exact-base root CAS.

No stable pack body is copied, downloaded, or uploaded merely to make a
checkpoint. The checkpoint keeps source runs and their capsules reachable after
clearing their transaction frontier entries. The current maintenance reader
uses the authenticated `CRBRUN04` control suffix, its embedded control bundle,
and its exact admission sidecar; it does not load frontier Git/file payload sections. Complete run
decoding remains reserved for strict fsck, history, and recovery paths.

A CAS loser leaves immutable unreachable objects, never partial visible state.
Normal GC removes them only after the grace period.

### 7.3 Checkpoint with geometric collision

Physical sources are weighted by compressed Git bytes, with member and object
counts retained for diagnostics. Using factor two, maintenance selects the
smallest source suffix whose combined weight violates the geometric
progression. It downloads only the pack members in that suffix and any
specific stable-prefix delta bases required to resolve them, produces one
verified standalone replacement layer, and publishes:

```text
stable prefix + replacement layer
```

The stable prefix is neither downloaded nor rewritten. If the current source
inventory is already geometric, repack is a metadata no-op.

This selection rule is deterministic for one pinned pack set. It must use the
existing suffix-consolidation mechanics rather than the current
`repack_repository_complete` call.

Logical checkpoint and physical repack are two separately published
transitions. The quick checkpoint first makes the bounded logical view
available. A later maintenance pass pins that checkpoint, builds the
replacement suffix, and publishes a second metadata checkpoint with the same
refs. A slow repack therefore does not hold checkpoint visibility hostage. The
only exception is the hard 64-source admission limit, where another source
cannot become visible until a bounded suffix roll-up succeeds.

Suffix construction uses this ordered strategy:

1. return a no-op when the inventory already satisfies geometry;
2. when selected member object sets are disjoint, structurally concatenate
   their committed pack entries, rebuild indexes and the locator, and compare
   the exact output object set without inflating or recompressing every object;
3. when duplicate objects or unresolved thin deltas prevent concatenation,
   range-read only the named external bases and run the bounded selected-suffix
   repack; and
4. reject the candidate unless its object universe and declared external-base
   closure exactly match the selected suffix contract.

The normal append-only push path is expected to take step 2. Force pushes and
resurrection may require step 3. Neither path reads a stable-prefix pack body,
runs complete-repository `git repack -a`, or performs full strict fsck as part
of checkpoint publication. Deep fsck remains a separately measured integrity
operation over already committed immutable bytes.

The fast path is a streaming `O(bytes(S))` operation: validate committed
member identities, copy complete pack-entry sequences while preserving their
internal `OFS_DELTA` distances, rebuild the combined header/trailer and
sidecars, and prove the output OID set equals the union of the inputs. It does
not inflate objects or run delta search. Peak memory is bounded by indexes and
the configured I/O buffers, not selected payload size. A member with an
external `REF_DELTA` base uses the same fast path only when the base remains
declared against the retained prefix or is included in the selected closure;
otherwise the bounded fallback reads and authenticates the named base before
invoking Git. The current maintenance path carries that target-to-base map into
the replacement layer and fails closed on a missing or mismatched closure.

### 7.4 Scheduling

Checkpointing and geometric repack are related but independent decisions:

- checkpoint when the bounded capsule frontier requires compaction;
- repack only when source geometry, source count, or measured clone debt
  requires it;
- never trigger complete repack solely because 500 more commits arrived; and
- enforce byte, disk, memory, and elapsed-time budgets before mutation.

If maintenance is behind, foreground publication may wait at the hard frontier
limit, but it must not silently execute an unbounded complete-repository repack.

## 8. Clone, fetch, and pull

### 8.1 Pinned read view

Every reader captures one root/ref snapshot, authenticates the metadata-only
checkpoint, and merges its locator with the bounded capsule frontier. Later
checkpoint publication cannot alter that view.

The target checkpoint contract carries an authenticated transition-to-source
and pack-member join. With that join, incremental fetch knows which new
sources can contain its selected objects without opening every stable source
control. CP05 now carries this exact join for the compacted checkpoint
visibility dictionary. Post-checkpoint frontier runs still expose only their
authenticated pack directory, so the reader probes those preferred frontier
indexes lazily and falls back to older indexes only for misses. Delta-base
resolution may consult older controls through the same bounded cache.

That is the target read contract. `CRBCKP05` authenticates the decision with
the compact ordinal proof and control-only frontier. Stable sidecar admission
and selected-range coalescing are live. A normal frontier hit uses the
authenticated OID-to-member admission directly; when a ref update reuses an
object from an older stable source, the reader performs one batched locator
join, then re-runs the same sidecar/object-set and external-delta proof before
installing the selected immutable members. The join is therefore a bounded
miss path, not a scan of every source, and there is no extra lookup on the
normal frontier-admitted path. The remaining hot-fetch cost is local Git work
(response-pack generation or connectivity-proof pack materialization when the
direct member path cannot prove closure); object-store request count is no
longer the dominant cost in the measured full-source runs.

Geometric replacement rebinds affected transition groups to the replacement
layer and member only after its exact object-set proof passes. Stable groups
retain their original source and member identity. A stale or incomplete rebind
is checkpoint corruption, never a reason to scan every pack body.

Arbitrary lazy-object and browser reads that lack transition provenance may
search source-local locators newest-to-oldest. The active inventory is bounded
at 64 physical sources and normally maintained at eight or fewer; controls are
loaded in one bounded parallel wave, and repeated reads use the immutable
cache. Qualification must report cold control requests separately.

### 8.2 Remote-helper clone and fetch

The line-oriented Git remote-helper `fetch` command supplies wanted ref tips,
not an upload-pack have negotiation. Crab must therefore derive candidate
haves from local refs and object availability. It accepts only tips that the
pinned checkpoint proves were visible predecessors of the requested ref, or
other advertised tips whose reachable closure is authorized for this read. An
arbitrary object found in the local object database cannot authorize omission
from the response.

After computing `wanted closure - authenticated local-have closure`, Crab
evaluates the exact selected object set:

1. derive the local pack filename from the authenticated pack-body identity;
2. reuse an already complete `.pack`/`.idx`/`.rev` triplet;
3. return no pack when every wanted tip and its required closure are already
   present, including a repack-only checkpoint transition;
4. directly install a missing source only when its complete object set is
   selected, none of it is satisfied by local haves, and doing so will not
   violate the local pack-count budget;
5. otherwise plan selected member-entry ranges and required delta bases, read
   them in one bounded parallel wave, then generate and install exactly one
   response pack; and
6. verify the requested tips and prove every selected dependency is either in
   the response or in the authenticated local-have closure before
   acknowledging the fetch.

After checkpoint `N`, an incremental fetch of checkpoint `N + 1` therefore
reads only objects introduced since the client's common tip. It does not
download a geometric replacement layer merely because maintenance gave those
already-present objects a new physical identity. A changed checkpoint identity
is not by itself a reason to read any pack body.

The legacy `CRBCKP03` path retains its complete-inventory installation contract
for compatibility. The `CRBCKP05` path instead keeps checkpoint sources
range-addressable, includes the visible post-checkpoint capsule frontier, and
lets the remote reader select only missing response objects. This prevents a
changed checkpoint identity from forcing every active source into the local Git
object database. Git's own later maintenance remains valid, but it is no longer
forced by the layered checkpoint path.

The range planner groups selected entries by physical source, sorts their
absolute ranges, and coalesces nearby ranges under an explicit extra-byte
budget. For a consecutive replay, selected capsules are adjacent inside a
run, so the normal result is one payload range per implicated run rather than
one request per object or pack member. It may choose a complete small member
or a larger contiguous envelope when that reduces requests within the byte
budget. Request minimization never changes object selection: extra bytes are
verified internal input and are not installed or exposed unless authorized.
If the selected graph cannot fit the qualification request/byte budgets, the
operation remains correct, emits the actual plan, and fails the performance
gate rather than weakening integrity or authorization.

The single response pack is self-contained. Raw entry reuse is allowed only
when all delta dependencies are included and structurally valid; otherwise the
selected objects are reconstructed and repacked. A thin response may be used
only through Git's `index-pack --fix-thin --stdin` contract with every omitted
base proven in the authenticated local-have closure. Crab never places an
unresolved thin pack directly in `.git/objects/pack`.

The direct-reuse implementation covers the exact one-member case. It checks the
authenticated member index and locator before downloading the pack, rejects any
unproven external `REF_DELTA` base, and then streams the verified member body as
the one Git response pack. For a selection spanning multiple members, the reader
first attempts the same proof per member and structurally concatenates the
complete, disjoint members into one response pack without inflating or
recompressing objects. A missing member, overlap, sidecar, or external-base
proof fails closed to the bounded selected-object response-pack path. This keeps
the optimization correct while removing the old multi-member CPU/RSS cliff;
the qualification gate still measures whether the selected source artifacts fit
the fetch budget and whether the resulting response meets the latency target.

Cold clone is different. With no local haves, Crab may install complete stable
sources in parallel when that avoids response regeneration, but the final
active source count remains bounded and every source is authenticated before
refs become visible.

### 8.3 Standard Git wire clone and fetch

Git upload-pack still emits one response pack. It never concatenates layer
files on the wire. The read module selects wants minus proven common haves,
resolves the selected objects and delta bases through the merged locator, and
generates one valid self-contained or negotiated thin response pack.

For an exact, fully authorized clone, a one-layer pack set may stream that
layer directly. A multi-layer pack set uses one of two non-authoritative
optimizations:

- generate the response from parallel layer reads; or
- reuse a verified response artifact keyed by the complete selection and pack
  set identity.

The artifact cache may improve cold clone but is never publication authority.
Missing or corrupt artifacts regenerate from the authenticated pack set.

### 8.4 Partial, shallow, lazy, browser, and mount reads

These reads use the same merged locator and authorization proof. They read only
selected pack ranges plus recursive delta bases. They cannot stream a complete
layer when its catalog contains unrequested or unauthorized objects.

Xet pointer hydration remains separate: the Git read yields pointer objects,
then the pinned pointer catalog resolves shards and xorbs through their
canonical object keys.

## 9. Correctness argument

### 9.1 Durable before visible

Every new or replacement layer and checkpoint is immutable and fully verified
before the root CAS can name it. Publication failure leaves the old complete
view authoritative.

### 9.2 Atomic pack-set replacement

One checkpoint names the complete ordered pack set. Readers never combine
layers from different checkpoints. Geometric repack changes one suffix only
inside the candidate checkpoint and exposes the replacement atomically through
root CAS.

### 9.3 Object and delta integrity

The checkpoint authenticates source identities and the locator directory. Each
source authenticates its pack and sidecars. The reader derives its merged view
only from those committed locators. Reconstruction checks CRC, delta-base
identity, Git object kind and size, and final Git object ID. Missing, ambiguous,
cyclic, or corrupt dependencies fail closed.

### 9.4 Authorization

Pack sets are storage acceleration, not visibility authority. The pinned ref
and visibility proof determines the allowed object closure before any direct
layer stream or selected range read.

### 9.5 GC safety

The mark set includes:

- every capsule run and standalone layer named by the current checkpoint;
- every source named by every retained history checkpoint;
- transitive pack-source dependencies;
- all current and retained capsules, shards, xorbs, LFS bodies, and activation
  evidence;
- coordinator-protected keys; and
- objects inside the immutable-reader grace period.

Sweep lists `v2/pack-layers/` alongside checkpoints, capsules, and history.
Old capsule sources and suffix layers remain until no retained checkpoint or
dependency names them and the grace period has elapsed.

Active efficiency and retained-history storage are separate accounting classes.
Physical repack rewrites only the active suffix; it never rewrites historical
pack sets. A source that is obsolete for the active view remains deliberately
retained while a kept history checkpoint needs it. `crab gc` and history
inspection must report active, history-only, grace-period, and collectible
source bytes independently. History pruning remains an explicit fenced policy
operation; maintenance may not silently discard recovery points to improve its
storage numbers.

### 9.6 Crash and cancellation safety

Every expensive phase is cancellation-aware before publication. Temporary
local state may be discarded. Uploaded immutable objects are harmless until
named. Once root CAS succeeds, all named objects were already verified and
durable. Lock and GC-fence release rules remain unchanged.

## 10. Request and byte model

The clean foreground push budget remains four qualified or five readback
operations after advertisement. Layering adds no foreground request.

For maintenance, let `S` be the selected geometric suffix and `P` the stable
prefix:

| Operation | Pack-body reads | Pack-body writes |
| --- | ---: | ---: |
| Checkpoint with no roll-up | Stable prefix: zero; current frontier controls may read full runs | Zero |
| Geometric roll-up | Sources in `S` plus required external bases | One replacement layer |
| Already geometric | Zero | Zero |
| Complete operator re-optimization | All layers | Replacement inventory |

Normal maintenance MUST perform zero body reads and zero body writes for `P`.
Control metadata reads may include the checkpoint and selected source
suffixes, but they must be measured separately from payload bytes.

For a warm incremental remote-helper fetch, old stable-source body reads MUST
be zero. Origin reads consist of mutable view capture, immutable
checkpoint/source-control misses, frontier runs, and selected new-object
ranges. Requests may remain greater than one, but transferred bytes must scale
with the Git delta rather than total repository size.

A repack-only checkpoint with unchanged requested tips has zero pack-body
reads, zero response-pack bytes, and zero new local packs. A non-empty
single-ref incremental fetch installs at most one new local response pack. The
same read-admission lease covers the complete fetch; pack-source fan-out cannot
acquire one lease per source.

For the Kubernetes 500-commit interval used by qualification, the performance
target is at most ten total origin operations for a warm single-ref fetch after
immutable control caches are warm, with no more than one sequential payload
read wave. The coalescer also has a measured byte-amplification ceiling; it
cannot satisfy the request target by rereading a stable GiB-scale source. This
is a release target, not a correctness shortcut: a workload that requires more
verified ranges reports them honestly and fails the performance gate rather
than transferring unauthorized or unbounded unrelated data.

Request count alone is insufficient. The fetch gate also measures source bytes,
local bytes written, number of input sources, response-pack generation CPU,
local validation CPU, parent Git automatic-maintenance time, and peak RSS.

## 11. Observability

Add metrics and qualification fields for:

- physical-source and pack-member counts and geometric debt;
- stable-prefix and selected-suffix source/member/byte counts;
- stable bytes reused, read, rewritten, and transferred;
- checkpoint metadata bytes versus pack-source payload bytes;
- per-source cache hits and misses;
- external delta-base reads and bytes;
- response-pack generation/cache strategy and time;
- maintenance CAS conflicts and orphan layer bytes;
- incremental fetch wants, authenticated haves, selected objects, raw and
  coalesced ranges, useful and extra payload bytes; and
- GC marked/deleted layer counts and bytes.

Fetch timing is split into view capture, transition selection, source-control
open, payload reads, response-pack generation, local pack validation/install,
tip/dependency proof, remote-helper wall time, and parent Git post-helper
maintenance. Qualification enables Git Trace2 so `git fetch` time after the
helper exits cannot be misattributed to object storage.

Repack timing is split into inventory selection, selected-source download,
external-base reads, disjointness proof, structural concatenation or fallback
recompression, sidecar construction, candidate validation, upload, and CAS.
Every phase reports CPU, wall time, bytes, and attempts.

The critical regression signal is `stable_pack_body_bytes_read > 0` during
ordinary checkpoint construction or a warm incremental fetch.

## 12. Implementation sequence

Each phase lands with one canonical path and focused proof. Later phases do not
ship while the earlier contract is bypassable.

### Phase 1: Freeze contracts

- Add bounded `PackSourceDescriptor`, `PackMemberDescriptor`, `PackLayer`,
  `PackLayerControl`, and `PackLayerPointer` codecs.
- Replace `CRBRUN02` with `CRBRUN04` aggregate member/locator control, exact
  OID-to-member admission, and a bounded control bundle so one physical run opens without nested control
  range fan-out. Inline small transaction/visibility/catalog sections, but
  keep larger sections as authenticated body ranges and fetch them only when
  materializing that control. The remaining trailer-discovery read is removed
  when the authenticated pointer carries the suffix range.
- Replace embedded checkpoint pack sections with ordered source descriptors in
  `CRBCKP05`.
- Make checkpoint decode prove valid source kind, object and body identities,
  unique ranges, ordering, counts, bounds, and dependency declarations.
- Bind transition object groups to pack-body identities so incremental reads do
  not probe every source locator.
- Add deterministic round-trip, corruption, truncation, oversize, duplicate,
  source/member-bound, and dependency-cycle tests.

### Phase 2: Add canonical storage paths

- Add the fan-out `v2/pack-layers/` path to `StoreLayout`.
- Classify the immutable object for cache and inventory accounting.
- Test exact key construction and prevent callers from formatting keys.

### Phase 3: Open layered read views

- Load the metadata-only checkpoint and capsule-run/layer controls under one
  pinned view.
- Build one merged locator and validate its object/dependency closure.
- Teach selective reads and local installation to resolve source members
  without automatically installing every missing active pack.
- Derive remote-helper local haves from local refs/object availability and
  authenticate them through the pinned visibility transition history.
- Use the authenticated run control bundle for transaction/ref materialization;
  range-fetch oversized control sections by their committed capsule ranges;
  coalesce selected absolute member ranges by physical source with explicit
  request and extra-byte budgets. Compacted-source admission is exact and
  authenticated; frontier transition binding remains a release gate.
- Prove stable local packs are skipped by body identity across a changed
  checkpoint.

### Phase 4: Publish layered checkpoints

- Fold eligible capsule-run descriptors into the pack set without copying
  their pack bodies.
- Upload the checkpoint and history before exact-base root CAS.
- Reconcile uncertain writes by exact immutable identity and transaction state.
- Prove checkpoint publication performs zero pack-body reads/writes, and CAS
  loss, retry, cancellation, and corruption cannot expose a partial pack set.

### Phase 5: Implement geometric suffix maintenance

The implementation now performs a bounded suffix roll-up: it retains a stable
prefix, coalesces and reads only selected source members, runs the verified Git
consolidation path (including its disjoint-pack structural concatenation fast
path), carries forward the exact generated external-`REF_DELTA` map, writes
immutable `CRBPKL01` layers, and publishes the replacement source directory.
The geometric cut operates on compressed-byte weights in publication order, so
it does not reorder duplicate-object precedence when a replacement layer is
larger than its immediate predecessor. A source-count bound still keeps the
active view below the hard 64-source limit (the steady-state target is eight).
The latest CP05 RustFS smoke measured 0.330--0.342 s and 27--28 requests per
interval roll-up on the bounded fixture; long-run 5,000-commit behavior and
frontier admission remain qualification gates rather than shipped claims.

- Reuse `incremental_repack_cut`/suffix consolidation with compressed-byte
  weights.
- Read only the selected suffix and explicitly required external bases.
- Use exact disjoint-source structural concatenation as the normal path and
  reserve delta recompression for overlap/thin-repair cases.
- Preserve the stable prefix verbatim and publish one replacement suffix.
- Prove no-op geometry performs zero pack-body reads/writes and suffix roll-up
  preserves the exact object universe.

### Phase 6: Complete every reader

- Replace unconditional remote-helper inventory installation with no-op,
  exact-member, whole-source, or one selected-response-pack decisions based on
  authenticated local haves. Exact-member response reuse is now live; it is
  admitted only when the selected object set equals one complete member and
  every external delta base is in the proven have set. The response path now
  also attempts a structural union when the selection is exactly two or more
  complete, disjoint members: catalog locators prove the partition, each
  member index proves external-delta closure, and the authenticated bodies are
  concatenated without inflating or recompressing them. Any overlap, missing
  member, sidecar, or delta-base proof falls back to the existing selected-pack
  writer; the optimization is therefore fail-closed. Frontier sidecar
  admission is now transition-driven and authenticated; the warm-fetch request
  target remains a full-repository qualification gate rather than an unproven
  implementation claim.
- Install negotiated thin responses only when the local repository is complete
  and the haves prove every omitted base. Use Git `index-pack --fix-thin` in a
  temporary path, validate all sidecars, and publish them atomically; shallow,
  partial, unreadable-config, missing-base, and index failure cases use the
  self-contained path or fail closed without leaving a pack artifact.
- Wire HTTP upload-pack, protected reads, browser, mount, partial, shallow, and
  lazy fetch to the same locator and selection implementation.
- Add response-artifact caching only after the canonical generated-response
  path passes correctness and memory bounds.

### Phase 7: Update fsck, history, recovery, and GC

- Make strict fsck validate all source and member hashes, sidecars, object IDs,
  external bases, visibility, refs, and external large-file dependencies.
- Retain source capsule/layer closure across current and historical
  checkpoints.
- Report active and history-only source bytes separately and preserve explicit
  fenced history pruning.
- Update history restore and metadata rebuild to publish the same layered
  format rather than synthesizing a complete pack.
- Add concurrent-reader, history-prune, forced-GC, orphan, and grace-period
  tests.

### Phase 8: Remove `CRBCKP03`

- Delete the embedded checkpoint-pack writer, complete-pack checkpoint
  consolidation, and tests that protect the retired shape.
- Reject old development repositories with an explicit format error.
- Do not retain a hidden fallback reader or migration adapter in normal paths.

### Phase 9: Qualify and decide retirement

- Run the release binary against isolated local RustFS and every supported
  hosted provider.
- Compare v1 and v2 on the same source revision, machine class, object-store
  placement, cache state, and harness.
- Keep v1 supported until every release gate below passes.

## 13. Verification matrix

### 13.1 Deterministic tests

Required automated proof:

- byte-identical layer/checkpoint encoding;
- corrupt body, footer, sidecar, locator, count, and dependency rejection;
- stable-prefix preservation across checkpoint publication;
- a metadata-only checkpoint performs zero pack-body reads and writes;
- exact object-universe preservation across suffix consolidation;
- disjoint selected sources take structural concatenation without object
  inflation or delta recompression;
- cross-layer `REF_DELTA` resolution and missing-base failure;
- no cross-layer `OFS_DELTA` acceptance;
- exact-member response reuse rejects unproven external `REF_DELTA` bases;
- thin response installation repairs only against a present authenticated local
  base, rejects a missing base, and leaves no partial pack sidecars;
- warm incremental fetch performs no stable-source body read and does not
  download a repacked copy of objects already proven by common haves;
- repack-only fetch returns no pack, and a non-empty incremental fetch installs
  at most one pack regardless of source count;
- default Git automatic maintenance does not turn one Crab fetch into a
  whole-repository repack;
- fresh, partial, shallow, and lazy clone/fetch produce valid Git packs;
- hidden refs never leak through direct-layer or selected-range paths;
- root/ref CAS races and lost responses reconcile without split visibility;
- retained history remains restorable after multiple roll-ups; and
- GC never deletes a pack source reachable by a checkpoint, history segment,
  dependency, reader grace period, or coordinator protection.

### 13.2 Kubernetes 5,000-commit RustFS gate

Current bounded CP05 evidence includes the source-aware-coalescing
Kubernetes-derived 20-commit smoke in section 2.5.3 (correctness passed,
252--259 ms fetches, 14 total origin operations) and the release
`CRBRUN04` exact-admission run in section 2.5.13 (correctness passed,
21.75--23.01 s fetches, 24/26 total origin operations on the full staged
source), plus the isolated v2 xorb/shard end-to-end smoke in section 2.5.14
(33/33 checks, byte-identical hydration and rehydration). The latter proves
the sidecar does not weaken the read contract, but also shows that
response-pack generation and local `index-pack` work remain the hot path. The
current-binary empty-bucket replay in section 2.5.15 also passed all 20 pushes,
two fetches, two repacks, final clone, and full fsck, with 21.95--22.69 s
fetches and 26/27 total origin operations. No bounded run satisfies the
ten-operation, ten-second, or 5,000-commit gates below.
Section 2.5.38 additionally records a fresh uncheckpointed-root clone: the
single-member direct path completed in 46.95 s for a 1.27 GiB pack and passed
native full fsck. This rules out the prior full-capsule cold-clone fallback,
but does not satisfy the few-second or 5,000-commit gates.

Use a fresh GitHub Kubernetes clone as the read-only source and an isolated
RustFS repository:

1. seed through v2 and publish the first layered checkpoint;
2. fresh-clone, run native `git fsck --full --no-reflogs`, and run strict
   `crab fsck`;
3. replay 5,000 first-parent commits as 5,000 individual pushes;
4. every 500 pushes, incremental-fetch into the same client, verify its tip,
   run geometric maintenance, and record requests, bytes, CPU, RSS, and time;
5. after commit 5,000, perform independent cold and warm clones, native and
   Crab fsck, and sampled byte comparison; and
6. retain raw request logs and machine-readable summaries.

The run passes only if:

- all 5,000 pushes and all ten incremental fetches succeed;
- push request count and p50/p95/p99 latency remain flat by replay window;
- mean simple-push object-store operations remain below ten;
- warm 500-commit incremental fetches use at most ten origin operations after
  immutable control caches warm and complete within 10 seconds p95 on the
  recorded reference host;
- no incremental fetch reads a stable source body already installed locally or
  downloads a replacement copy of objects already proven locally;
- each ordinary checkpoint/repack reads and writes only its frontier or
  selected suffix;
- pack-source count stays within the geometric bound;
- each non-empty incremental fetch installs at most one local pack and Git
  Trace2 attributes no hidden whole-repository automatic maintenance to it;
- incremental transferred bytes track the 500-commit delta rather than total
  repository size;
- final fresh and warm clone performance is no worse than v1 under the same
  harness;
- native Git fsck, strict Crab fsck, refs, and sampled file digests pass; and
- no xorb/shard/LFS dependency is lost, embedded, or collected early.

Absolute latency claims are reported, not inferred from local RustFS. Hosted
qualification must separately prove WAN p50/p95/p99 and throughput.

### 13.3 Failure and concurrency gate

Inject cancellation, timeout, lost response, stale CAS, corrupt range, missing
base, concurrent same-ref/disjoint-ref push, checkpoint race, history restore,
normal GC, and forced GC at every publication phase. Every result must be one
complete old view or one complete new view; a mixed pack set is a release
failure.

## 14. Rollout and rollback

Development repositories using `CRBCKP03` are recreated or converted by an
explicit offline tool after their source repository is retained. Normal Crab
commands do not translate formats opportunistically.

The new format remains unreachable from a release tag until deterministic,
RustFS, and hosted-provider gates pass. Rollback before format activation is a
binary rollback. After a repository is initialized with the new format,
rollback means restoring the retained authoritative source into a separately
initialized supported repository; an older writer must never mutate the new
layout.

Protocol v1 retirement is a separate decision. It requires v2 to beat or match
v1 on the identical production-qualification matrix while preserving all
correctness and product-parity gates.

## 15. Why this is the best fix

Reducing checkpoint frequency only postpones the whole-repository rewrite and
makes the frontier larger. Moving an unchanged monolithic checkpoint pack to a
new object key still changes or redownloads its identity. Adding more caches
hides the cost only for warm readers and cannot repair maintenance
amplification.

Stable layered packs move ownership of incremental reuse into the authenticated
storage contract. They give checkpoint, fetch, clone, repack, history, fsck,
and GC one shared invariant: unchanged Git bytes keep the same immutable
identity. That is the deepest and most leveraged seam, and it matches the
incremental pack behavior already demonstrated by v1 without reintroducing
v1's foreground metadata fan-out.
