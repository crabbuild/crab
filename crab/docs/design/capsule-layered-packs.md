# Protocol v2 Stable Layered Packs

## Document metadata

| Field | Value |
| --- | --- |
| Project | Crab |
| Scope | Protocol-v2 checkpoint Git packs, clone/fetch, repack, fsck, history, and GC |
| Status | Working implementation, not qualified. The [retained exact-head GA replay](../benchmarks/capsule-v2-kubernetes-5000-rustfs-ga.md) on `53b11070` completed 5,000 Kubernetes pushes, ten exact-tip fetch/repack intervals, cold/warm clones, strict Git/Crab integrity and sampled blob comparisons, but fetch p95 was 11.068 seconds / 34 requests and clones took 48.683 / 28.526 seconds. A new CRBRUN07 per-ref rollup is locally covered by 236 metadata, 219 reader and 31 writer tests, including 1,000 sequential publications; it has not yet been replayed on RustFS. PR #208's current baseline head is `c3ce1439`, so the final candidate needs a fresh 5,000-push replay. Exact-head 100 GiB Xet, hosted providers, full product parity, paired v1 and v1 retirement remain open. |
| Priority | Correctness, stable incremental cost, then clone throughput and storage efficiency |
| Replaces | Whole-repository Git-pack replacement during every v2 checkpoint |
| Companion | [Capsule Publication Protocol](capsule-publication-protocol.md), [Protocol v2 Xorb and Shard Integration](capsule-xorbs-shards.md), [Kubernetes 4,500-commit RustFS benchmark](../benchmarks/kubernetes-4500-rustfs.md) |

An earlier bounded-frontier replay stopped after 1,112 of 5,000 incremental
pushes because the shared qualification volume ran low. Its two 500-commit
fetches preserved the exact tips and installed one new pack each, but took
11.565/10.910 seconds and 32 requests each. The [retained GA qualification
record](../benchmarks/capsule-v2-kubernetes-5000-rustfs-ga.md) distinguishes
this capacity stop from the later complete replay. Fetch request performance,
Xet, and parity gates remain open.

The current-head GA trace's final fetch used one complete GET for each of 24
distinct new capsule-run objects, plus ten root, ref-capture, admission,
replica-discovery and checkpoint operations. Earlier traces had eight such
control operations. Reader-side range coalescing is already at the
one-request-per-source floor for this interval. Changing only the 32-leaf
compaction fan-in to four produced six sources and 14 total requests in the
matched diagnostic below, while increasing upload bytes. Meeting ten needs
both less source fan-out and cheaper coherent control capture: even one source
plus the current ten control requests would miss the gate.

The exact-head commit-5,000 trace accounts for all 34 requests:

| Request group | Count | Observed operations |
| --- | ---: | --- |
| Capsule sources | 24 | One authenticated full-object GET per distinct run |
| Root and replica routing | 2 | Root GET; replica-discovery GET returning 404 |
| Read admission | 4 | Conditional create returning 412, GET, conditional update, release PUT |
| Ref capture | 3 | LIST, selected ref-head GET, second LIST to detect a changed set |
| Checkpoint control | 1 | Authenticated suffix range GET |

The 404 and 412 are expected protocol responses but still incur requests.
The double listing protects the captured ref set; the admission lifetime
protects read/GC coordination. A lower-request path must replace those proofs
with equivalent coherent capture and release, not omit them or exclude their
requests from the meter. The source and control budgets must be evaluated
together on a fetch-before-repack replay, including conflicts and retries.

A subsequent matched 500-commit Kubernetes/RustFS diagnostic confirmed that
four-way compaction produced six sources and 14 total fetch requests, versus
24 sources and 32 requests with 32-way compaction. Fetch took 6.079 versus
4.423 seconds, while average push requests rose from 7.012 to 7.488. Both
variants passed exact-tip, clone, fsck, and sampled-byte checks, but both
failed the unchanged ten-request fetch gate. This is one sequential local
timing pair, not proof of a causal latency regression or remote-store behavior.
The fan-in-only experiment was reverted; see the [matched diagnostic](../benchmarks/capsule-v2-kubernetes-5000-rustfs-ga.md).

At 05:28 UTC on September 28, a separate cleanup removed the earlier mounted
qualification directories and most of a fresh live replay's working files.
That replay had reached 879 pushes, but its next push could not start because
its binary link was gone; only a failed report and a truncated request log
survived. A new sibling-directory run copied the binary locally and completed
all 5,000 pushes and correctness gates; its raw report and request log remain
inspectable. Its fetch request gate still fails at 34 p95, so this is not
release or v1-retirement qualification. The benchmark record separates this
result from earlier lost-artifact and stopped runs.

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

This is the required structural fix. Merely putting the complete pack
under a separate object key would not help: the pre-layered maintenance path
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

### 2.1 Pre-layered v2 behavior

The pre-layered checkpoint module:

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

The direct path is deliberately fail-closed. Missing admission,
conflicting member evidence, an unproven object, an external base, a shallow
or filtered request, or any sidecar/range integrity failure uses the existing
verified response-pack path (or returns the underlying corruption error).
Byte-identical repeated members retain their authenticated source positions
and share one verified local installation. Stable local pack bodies are skipped,
and source ranges are coalesced within
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

### 2.5.46 September 21 authenticated multi-member cold-clone fix

The multi-member cold-clone failure above was traced to the complete layered
reader keeping every member index lazy. Filtered planning therefore fell back
to visibility traversal and, for the Kubernetes-derived fixture, issued
millions of small object reads while materializing a response pack. The fix
keeps the cheap footer/tip-bound view for ordinary incremental fetches, but
promotes a cold clone or a request with filter, shallow, or tag semantics to a
complete layered view. That view coalesces each source's authenticated index,
reverse-index, and kind-bearing locator ranges, verifies each range hash, and
builds inline object locators before Git planning. No pack body is loaded just
to answer incremental haves.

The local-RustFS requalification used the same 5,000-commit Kubernetes-derived
fixture and the PR-208 release binary. A filtered `blob:none` clone reached the
source tip in 14.50 s with catalog planning in 173 ms; a cache-miss shallow
`blob:none` clone completed in 6.12 s, with 1,095 planning reads and 372
terminal response-pack reads. An unfiltered multi-member cold clone completed
in 85.94 s, reached the exact source tip, and passed native `git fsck --full`.
The response pack was 1.12 GiB; the remaining wall time was local Git
pack/index installation, not the previous millions-of-range-read visibility
fallback. The three clone destinations (full, filtered, and shallow) all
matched the source tip and passed full fsck.

This closes the previously observed complete-view request-amplification path,
but it is not a blanket few-second or 5,000-push qualification claim. The
fresh 5,000-commit replay, repeated fetch/repack matrix, hosted-provider
latency, and v1-retirement gates remain open until they are run on the final
branch and recorded below.

### 2.5.47 September 21 protocol-v2 negotiation haves retention

The first multi-member requalification exposed a negotiation-state bug after
the cold-clone promotion fix: Git protocol-v2 can send haves in several fetch
rounds and omit them from the terminal `done` request. The wire server was
therefore replacing the authenticated frontier with an empty terminal list,
which selected the complete repository for an ordinary incremental fetch. The
server now merges and de-duplicates haves before view promotion and copies the
complete set into the terminal request. This preserves the fail-closed proof
while keeping incremental selection bound to `wants - haves`.

A non-shallow full-history Kubernetes source on local RustFS passed incremental
fetches after pushes 1 and 5, with exact remote tips and no missing-object
errors. The responses were 49.9 MiB and 55.7 MiB; the earlier faulty path
returned a 1.09 GiB complete pack for the same class of request. The remaining
15,296--17,127 range reads and 16.2--19.9 s wall time are current performance
data, not a release claim; locator-read coalescing and the 5,000-push gate are
still open.

The replay subsequently reached a source commit carrying a 503,980,520-byte
Crab/Xet pointer and stopped with `CRAB-E0086` because the replay harness had
not staged that pointer's local chunks. That is a staging-contract qualification
failure, not an accepted fetch result: large-file replay must run through the
normal `crab add` staging path before it can close the xorb/shard gate.

### 2.5.48 September 25 skip the redundant fast-forward visibility walk

The capsule push path already proves that an existing branch update is a
fast-forward before accepting it. Visibility construction now reuses that
per-ref proof: `new - old` is still enumerated exactly, while `old - new` is
known to be empty and is no longer walked. Rewinds, tags, new refs, and updates
whose ancestry cannot be proven retain their complete prior behavior.

On a fresh full Kubernetes checkout at `6384b87ed0bef8bc893d2d4fd7ab93a1ce0fc2e1`,
the otherwise-empty `git rev-list --objects HEAD~5000 --not HEAD~4999` took a
71.2 ms median over five runs (64.4--84.2 ms after the first warm-up). This
shows the duplicated graph walk is measurable, but does not predict the total
push speedup. The focused capsule-push suite passes 11/11; Xet upload time,
provider readback, and the final 5,000-push RustFS replay remain unqualified.

### 2.5.49 September 26 retained 5,000-push audit

The retained run `pr208-01e512a-k8s-5000-20260926-r1`, using binary SHA-256
`011251f319939b5475a12bbdd8b20f4cf3f55e97f7ff142e45671b3cd81303c0`, completed
the seed, all 5,000 individual pushes, and ten fetch/repack intervals. Excluding
the seed, pushes averaged 287.3 ms and 7.988 object-store requests; latency
p50/p95/p99 was 235/577/886 ms, with 37 pushes above one second and a 4,192 ms
maximum. These are retained-binary measurements, not qualification of the
current working tree's admission-reuse, visibility-walk, or fan-in changes.
Trace2 for the slowest push (ordinal 343, six storage requests) attributes
1.352 s to `pack-objects`, 0.472 s to ancestry checking, and 0.460 s to strict
`index-pack`; fewer storage requests alone cannot remove that latency tail.

Qualification failed at final Crab fsck with `CRAB-E0030` for a retained
1,252,353,726-byte capsule. Isolated full-range reads of that object succeeded;
the failure must not be classified as deleted data or a proven provider bug.
Incremental fetches also still required 526--1,477 requests and 4.6--10.2 s;
interval repacks took 7.5--12.8 minutes. Final cold/warm clones took 143.1/151.2 s.
Neither the request/latency gates nor v1 retirement are satisfied by this run.

A focused fsck regression reproduced four complete reads of one immutable run
shared by three checkpoints and retained history. History verification now
collects unique physical sources across checkpoints, rejects conflicting
descriptors, and joins retained run pointers before reading source bodies.
It still applies the canonical run-pointer checks and complete descriptor/body
verification. Pack-layer member-range hashing remains unchanged. This removes
the nested checkpoint/source read fan-out; it is not a streaming-memory bound
or proof of the original RustFS failure's cause. All 43 focused
`cmd::fsck_store` tests and 11 capsule-push tests passed in the no-default-feature
test build. A fresh no-default-feature release binary (SHA-256
`c79493645288279d39907fe128a62435b9c13ca2b34b4633c1be7b5204dfac82`) then passed
read-only Crab fsck against the retained 5,000-push repository: zero errors and
repairs, 204.012 s, 125 successful HTTP GETs (120 object reads and five lists),
4,179,775,406 response bytes, and 3,841,638,400 bytes peak process-tree RSS.
There were no remote writes and no repeated 404 in this run. Diagnostic
artifacts are retained under run `capsule-history-verify-ba7qcI`; this is one
successful fsck replay, not a fresh full-workload or provider-parity result.

Remaining push work should prioritize compaction body-copy bursts and repeated
local Git graph/pack work, then qualify provider checksums before considering
removal of readback. Increasing fan-in alone postpones body work and creates
larger bursts and wider reader frontiers; push and fetch must be measured
together. Xet catalog loading and sequential built-xorb/shard publication
need separate large-file measurements; Git-only replay cannot qualify them.

### 2.5.50 September 26 fresh Docker replay and harness coverage

Run `candidate-c794936-k8s-5000-docker-20260926-r1` started against an empty,
isolated Docker RustFS `1.0.0-beta.8-glibc` bucket using the release binary from
section 2.5.49 and the same pinned, full-history Kubernetes source. It requests
all 5,000 individual pushes and all ten 500-commit fetch/repack intervals;
completion and performance qualification remain pending. Binary, harness, and
request-proxy hashes are recorded with the run.

The harness now verifies the seed clone's exact tip, strict native Git fsck,
and remote Crab fsck before any incremental push. Final results preserve those
seed checks. Fetch uses ordinary Git maintenance defaults, isolated from host
global/system configuration, rather than `--no-auto-maintenance`. No-op automatic
maintenance checks are recorded, while any fetch-time repack or removal of an
already installed pack fails verification. The focused harness/proxy suites
pass 35 tests, including regressions that first failed for omitted seed checks
and an undetected single-pack replacement.

The retained older run also exposes a telemetry gap: at push 4,000, repack's
structured summary reported zero bytes read/written despite transport measuring
324,001,989 response bytes and 120,673,398 request bytes. The CLI currently guesses
body I/O from source count, and layered view totals omit the live frontier.
These summaries do not prove metadata-only maintenance or complete inventory
bounds; source-selection accounting needs correction. Raw transport paths and
bytes remain the authoritative I/O evidence for the active replay.

At the 1,500-push observation, the seed's strict native Git fsck and remote
Crab fsck had passed, as had all three incremental fetch tip/connectivity and
pack-inventory checks. The first two 500-push windows averaged 262/247 ms,
with p95 529/453 ms, p99 743/678 ms, and 7.012 requests per push in each
window. Fetches took 9.220/4.602/16.296 s and 580/806/580 requests. The first
two interval repacks took 83.332/132.493 s; the third remained active. These
are partial observations, not a passing replay. An unrelated Clippy build
was observed on the host during the third window, so latency changes cannot
be attributed exclusively to Crab or commit count.

The second fetch's transport log contains 799 ranged GETs but only 580 unique
object/range pairs: 219 reads repeat an earlier range. Source inspection found
that batched packed-entry reads reopen a pack index to translate an OFS delta's
base offset even when the authenticated batch locators already identify that
base. The runtime retains at most 256 pack indexes by default, fewer than this
500-push frontier's possible member count. A focused reader regression now
requires a selected OFS base to resolve within a one-request body-read budget.
The test is formatted but has not yet been executed, and no corresponding
production change or speedup is claimed. Compilation is deferred until the
live replay exits to avoid adding benchmark contention. This explains a
concrete redundant-read path, not necessarily every duplicate request.

### 2.5.51 September 26 maintenance enumeration audit

The same Docker replay reached 2,500 pushes with five successful incremental
fetch tip/connectivity and pack-inventory checks. All five push windows averaged
7.012 requests; mean latency was 262/247/382/268/250 ms. Completion, final
clone/fsck, and the performance gates remain pending. The fifth fetch took
13.767 s, so sub-second push means do not imply qualified end-to-end throughput.

Git Trace2 identifies the dominant maintenance cost more precisely than total
repack time:

| Push interval | Repack wall time | Git object enumeration | Git pack preparation | Git pack writing |
| --- | --- | --- | --- | --- |
| 500 | 83.332 s | 71.412 s | 2.789 s | 0.468 s |
| 1,000 | 132.493 s | 118.719 s | 1.553 s | 0.427 s |
| 1,500 | 157.085 s | 146.434 s | 0.751 s | 0.543 s |
| 2,000 | 176.084 s | 165.539 s | 0.782 s | 0.722 s |

The overlapping-source branch in `crates/crab-git/src/repack.rs` invokes
`git pack-objects --stdin-packs`. In the measured Git 2.50.1 implementation,
[`read_packs_list_from_stdin`](https://github.com/git/git/blob/v2.50.1/builtin/pack-objects.c)
walks revisions/trees to populate best-effort packing name hints. This happens
even though Crab has already collected the exact source OID union. The next
experiment should feed that verified union directly to Git, preserving source
integrity, exact output-set validation, and external-base closure. Removing name
hints can change compression/layout, so output bytes and clone behavior must be
measured alongside maintenance latency. Reducing delta-search settings alone
does not address the observed enumeration cost. No speedup is claimed yet.

The qualification harness now records completed `pack-objects` phase counts,
summed durations, and maxima for each repack. Sums are per-process phase time,
not an additional end-to-end or parallel wall-time measurement. A regression
exercises the repack report path and failed before the field was added; all 36
focused harness/proxy tests pass. The parser also reproduces the four measured
Trace2 breakdowns above. This harness edit does not change the already-running
replay or its recorded startup provenance.

### 2.5.52 September 26 exact-union maintenance and selected-base reuse

The overlapping-suffix consolidation path now feeds the sorted, unique OIDs
from verified source indexes directly to Git. Maintenance, response generation,
and external-base repair share that input path. Source integrity checks remain;
the generated index must match the exact union, including in response mode.
Disjoint structural concatenation and explicit thin-base repair remain intact.
The real overlapping-pack fixture exposed `--stdin-packs` in all three modes
before the fix; its Trace2 regression now passes. All 16 focused repack tests
pass, including native Git reconstruction after cross-pack base replacement.

A mechanism probe captured the live 3,000-push suffix's 501 immutable input
packs (108,728 entries, 108,563 unique OIDs) without changing the live run. Git's
old enumeration phase took 222.567 s inside a 236.740 s Crab repack. Exact-OID
input reduced native enumeration to 0.148 s and the native command to 2.34 s;
the output index equals the source union and native `git verify-pack` passes.
The output is 62,005,700 bytes versus 57,103,230 bytes for the prior replacement,
about 8.6% larger. Raw Trace2, the retained inputs, output pack/index, hashes,
and `repack-3000-exact-oid-probe.json` are retained with the qualification
artifacts. This isolates the Git mechanism, not end-to-end Crab improvement.

The packed-entry reader also reuses authenticated batch locators for a selected
OFS base rather than reloading its index after cache eviction. Unselected bases
still use verified index resolution; conflicting OIDs at one pack offset fail
closed. The one-request regression first failed at request two, then passed
after the change. The 22 existing/focused reader tests, 28 pack-generation tests,
new conflicting-offset test, and native-Git OFS/CRC/corrupt-response integration
checks pass. Large-frontier fetch performance still needs a rebuilt-binary run.

The initial 5,000-push replay continues on its unchanged binary as a correctness
baseline. After the 2,500-push observation, focused single-job builds and the
native probe ran on the same host. Later timings are contended, not controlled
performance evidence. The replay already exceeds the fetch request gate; a
separate clean run of the changed release binary is required. Full completion,
v1 comparison, failure/concurrency, and product-parity gates remain open.

### 2.5.53 September 26 source-window index reads

The live baseline's first 500-commit fetch used 580 requests. Its capsule reads
comprised 500 member-index ranges, two control ranges per physical capsule,
and one payload range per capsule, across 24 capsule objects. No repeated
index range was needed in this first interval: cache enlargement alone cannot
remove that initial request amplification. The later selected-OFS-base fix
addresses additional repeated reads, not these compulsory index misses.

The batch locator path now coalesces nearby lazy indexes by physical source.
Windows use the existing 64 MiB source-range, 64 KiB gap, and 4 MiB overread
bounds, plus a 256-index cap. Each index retains its own BLAKE3, Git index
checksum, inventory-count, and offset validation. A corrupt sibling prevents
the entire batch from entering the parsed-index cache. Concurrent identical
windows reuse the existing shared-read admission and cancellation machinery;
each participant accounts for actual response bytes, including gaps. Standalone,
inline, and one-index reads retain their existing verified path.

The new two-index/one-request regression failed before the change at request
two and passes after it. All 28 reader tests and 28 response-pack tests pass,
including new corrupt-sibling, gap-limit, byte-budget, and independent-budget
checks. The native Git OFS integration also passes. The runtime group first
passed 15/16: its unchanged one-millisecond negative-cache expiry test expired
before the first assertion on the busy host. All 16 pass in a serial rerun;
the initial failure is retained here, not treated as a clean parallel-suite pass.
Strict Clippy initially stopped in unchanged `crab-storage` code at three
existing lint findings. A dependency-excluding check reached the changed
reader, then failed on existing type-complexity and constructor-argument
findings in `pack.rs` and `repository.rs`; the lint gate remains open. A
separate ten-case planner test passes the exact/excess gap, span, overread,
member-count, large individual index, and different-source boundaries.

An offline replay of the first interval's index ranges predicts 158 bounded
index windows instead of 500 requests. This trades 1,066,600 useful index bytes
for 10,042,882 fetched window bytes, including 8,976,282 gap bytes. These are
planner estimates from recorded ranges, not rebuilt-binary latency evidence.
Even perfect coalescing cannot satisfy the ten-request fetch gate while 24
uncached payload objects remain: publication/maintenance must reduce physical
source count as well. The full replay and a separate changed-binary run remain
required; no qualification or v1-retirement claim follows from these tests.

### 2.5.54 September 26 completed baseline qualification

Run `candidate-c794936-k8s-5000-docker-20260926-r1` completed all 5,000
individual incremental pushes and ten 500-commit fetch/repack intervals.
Seed and final remote Crab fsck passed. Both final clones matched the pinned
Kubernetes tip, passed strict full native Git fsck, and reproduced all 32
sampled source blobs. Incremental fetches preserved existing local packs,
installed at most one new pack each, and triggered no native Git repack.

The run still **failed qualification**: fetch p95 was 17,689 ms and 1,535
requests, above the unchanged 10,000 ms / ten-request gates. Pushes averaged
328.86 ms and 7.012 requests, with p50/p95/p99 of 246/742/1,289 ms. Interval
repacks consumed 2,267.844 seconds versus 1,644.281 seconds for incremental
pushes. Final cold/warm clones took 198.463/268.469 seconds. The host was
contended, so these are retained observations, not a controlled v1 comparison
or proof of flat latency. The newer exact-OID, selected-base, and coalesced
index-window changes were not in this baseline binary and require their own
full replay. Correctness success here does not retire any remaining release
gate or v1.

### 2.5.55 September 26 independent physical maintenance and duplicate sources

Logical checkpointing now opens authenticated source controls and publishes
metadata without geometric repacking. The hard 64-source limit alone may force
the minimum admission suffix roll-up. CLI repack, the generation owner, HTTP
background maintenance, and S3 background maintenance then pin the published
checkpoint independently for physical work. Foreground server admission remains
logical-only. A physical pass preserves checkpoint refs, transaction positions,
history, and generation, leaving newer ref heads untouched; cancellation,
corruption, and losing the root CAS cannot publish a partial replacement.

Three new regressions failed before repair: a duplicate suffix pack could remove
a stable member and invalidate its ordinal admission; a suffix with one unique
pack was rejected; and pack-hash selection could read an identical stable-prefix
body. Maintenance now passes only selected source descriptors to the shared
installer, retains the prefix verbatim, and permits the canonical verified
consolidator to receive one deduplicated pack. This also removes the synthetic
checkpoint previously used to install an uncheckpointed suffix. All ten focused
checkpoint integration tests pass, including native Git reconstruction/fsck,
concurrent ref publication, stale root, cancellation, source corruption, and
65-source admission. The changed-binary replay and consumer gates remain open;
these tests do not qualify long-run performance or replace section 2.5.54.

### 2.5.56 September 26 shared-reader and ref-only compaction qualification

Consumer compilation exposed a non-`Send` future in concurrent layered range
reads: the stream retained a lazy iterator over borrowed request descriptors.
A spawned-reader regression reproduced the compiler failure. Range descriptors
are now owned before suspension; the regression passes without changing range
selection, concurrency, or integrity checks.

A real checkpoint-plus-new-push fixture also reproduced missing frontier packs
in inventory totals. Layered pack count, bytes, declared object count, and
visibility identity now share the checkpoint-plus-frontier member inventory,
deduplicated by physical source identity. Complete and control-only views pass
the same regression. All twelve focused checkpoint integration tests passed,
including a new byte-budget test that observes only sidecar traffic before an
over-budget payload is rejected and confirms no local pack was installed.

The HTTP consumer then compiled, but two maintenance tests failed because
compacting a pack-bearing run with a ref-only run discarded exact admission.
Empty runs omit their empty sidecar; merge had mistaken that for missing pack
evidence. A codec regression reproduced the loss. Merge now preserves admission
only when the other run's authenticated pack directory is empty. Both orders
are tested, and an unproven pack-bearing sibling still prevents a complete
proof. All ten focused run-codec tests and nine HTTP maintenance tests pass.
The initial S3 rerun passed three of five cases; two cases still asserted the
embedded-checkpoint accessor after layered publication. The fresh release replay
remains pending; none of these component results qualifies the full design.

### 2.5.57 September 26 behavioral qualification of layered consumers

With explicit approval to replace obsolete-shape assertions, the reader's
sixteen-window expectation is replaced by the real incremental-install budget
fixture in `crates/crab-remote/tests/checkpoint.rs`. It observes only authenticated
sidecar bytes before budget rejection, confirms no local pack was installed,
then retries the same view with sufficient budget and verifies both original
blob bodies through native Git. This tests the actual admission boundary rather
than prescribing an uncoalesced request layout.

The two S3 mutation cases now assert exact layered source preservation,
checkpoint refs and transaction positions, and continued content reads. The
sustained-write case additionally proves the next mutation advances the visible
ref while remaining outside the prior checkpoint. All 30 focused capsule reader
tests, twelve checkpoint integration tests, and five S3 capsule tests pass.
The shared publication suite also passes 20/20 and the CLI capsule-push suite
11/11. These are component results; provider, concurrent-agent, latest-binary
5,000-push, paired-v1, and full product-parity qualification remain open.

### 2.5.58 September 26 completed fresh candidate replay

The fresh no-default-feature release build succeeded and is pinned in run
`candidate-layered-20260926-r2` with SHA-256
`352adfb965a5bb0f3a4aacee1a6990698422eba7e36b5f1435defabee447b75e`.
The replay uses the same read-only Kubernetes source revision as section
2.5.54, a new repository prefix, and the owned Docker RustFS instance. The
17 replay-harness and 19 request-proxy tests pass; workload counts, request
limits, and latency gates are unchanged.

Initial observations: seed push 183.794 s / nine requests; seed maintenance
5.961 s / nineteen requests; initial clone 13.526 s / eleven requests. Seed
maintenance retains the 1,099,723,385-byte pack rather than rewriting it and
records 210,424,682 response bytes, compared with 1,323,172,637 in the retained
baseline. Those bytes include source controls, visibility, and checkpoint
reads; they are not all pack payload. Seed native strict full Git fsck and
remote Crab fsck passed; the latter took 109.361 s, and the cloned seed tip
matched the source.

The first 500 individual pushes averaged 263.322 ms and 7.012 requests. The
500-commit fetch completed with the exact tip, preserving existing local packs
and installing one new pack: 4.566 s / 238 requests versus the retained
baseline's 9.220 s / 580 requests. Response bytes rose from 65,996,996 to
74,976,136, consistent with the coalescer's explicit gap-byte tradeoff. This
still fails the unchanged ten-request fetch gate. All 230 capsule requests
address distinct source/range pairs across 24 physical capsule objects. There
are no repeated ranges in that interval; increasing an in-process cache alone
cannot remove those compulsory first reads.

The first interval repack took 11.683 s / 94 requests versus 83.332 s / 89
requests in the retained baseline. Trace2 attributes 77.942 ms to object
enumeration, 922.168 ms to pack preparation, and 214.864 ms to pack writing.
Total response bytes rose from 191,480,700 to 270,256,792; independently pinned
logical and physical maintenance are not a request/metadata-byte win. The CLI
pack-body byte counters still use an inventory-size estimate and reported more
than total observed transport bytes, so they are not qualification evidence.
The proxy's actual transfer measurements remain authoritative.

The unchanged binary completed the entire run at `2026-09-26T08:28:26Z`:
all 5,000 individual pushes, ten incremental fetches and repacks, independent
cold and warm final clones, native strict full Git fsck, remote Crab fsck,
and 32 sampled blob comparisons in both clones passed. Both final tips equal
the source revision `6384b87ed0bef8bc893d2d4fd7ab93a1ce0fc2e1`. Every fetch
retained the existing local packs and installed one new pack. Git's default
automatic maintenance ran, but Trace2 recorded no incremental-fetch repack.

| Measurement | Retained baseline (2.5.54) | Fresh candidate |
|---|---:|---:|
| Push mean / p95 / p99 | 328.86 / 742 / 1,289 ms | 257.03 / 488 / 756 ms |
| Mean push requests | 7.012 | 7.012 |
| Fetch mean / p95 | 9.608 / 17.689 s | 5.467 / 8.222 s |
| Fetch mean / p95 requests | 780.8 / 1,535 | 248.2 / 291 |
| Ten interval repacks, total | 2,267.844 s | 152.291 s |
| Final cold / warm clone | 198.463 / 268.469 s | 141.781 / 64.190 s |

Each 500-push window averaged exactly 7.012 requests; mean latency ranged
from 241.55 to 281.34 ms. Fifteen pushes exceeded one second; the maximum
was 1.431 s. The 4,850 ordinary pushes used six requests each and averaged
247.86 ms. The 150 compaction pushes used 39--42 requests and averaged
553.61 ms; those 3% of pushes still produced 63.68% of upload bytes and
77.26% of download bytes. This fixes neither foreground compaction copy
amplification nor all tail latency.

Interval maintenance took 11.683--22.035 s, retaining two or three physical
packs. Native object enumeration took 76.946--153.215 ms: the prior
enumeration bottleneck is removed in the real replay, but total maintenance
latency is not flat. The tradeoff is measurable: interval maintenance requests
rose from 899 to 948, upload bytes from 977,702,604 to 1,404,723,084, and
download bytes from 2,822,320,578 to 3,613,856,505. Incremental-fetch response
bytes also rose from 601,694,064 to 703,191,231. Lower request count and CPU
cost must not be described as lower transferred bytes.

The final cold clone used 194 requests, including 147 multipart-part uploads
for its generated response artifact. Trace2 measured 0.723 s enumeration,
23.659 s preparation, and 17.992 s writing in native selected-pack generation;
the process-tree peak RSS was 2,121,072,640 bytes. The warm clone used 30
requests, hit the published artifact, and ran no `pack-objects`; native Git
still indexed the response, checked connectivity, and checked out files.
Final remote Crab fsck passed in 156.126 s / 306 requests. These clone results
are improvements, not the required few-second result.

The harness correctly exited with `status: failed`: push mean latency and
request gates passed, and the 500-commit fetch p95 latency gate passed, but
fetch p95 requests remained 291 against the unchanged limit of ten. This
is complete replay correctness evidence, not full release qualification.
Other host jobs were observed, so neither the timing comparison above nor
this run proves superiority over a controlled paired v1 run.

Remaining priorities exposed by this run:

- Remove foreground capsule-body recopying while retaining exact per-ref CAS,
  durable-before-visible publication, history, and reader/GC closure. Merely
  making compaction metadata-only would worsen fetch fragmentation unless
  paired with bounded physical maintenance.
- Reduce compulsory source/sidecar reads. Twenty-four uncached physical
  capsule objects in the first interval already preclude a ten-request fetch;
  enlarging a cache or relaxing coalescing byte bounds is not that solution.
- Keep multi-pack cold clones off full selected-response regeneration without
  weakening object-set authorization or external-delta-base verification.
  The current single-pack direct wire path cannot admit the final three-pack
  inventory. Section 2.5.59 isolates the overlap and records the bounded
  structural-response fix; avoiding receiver-side full indexing remains open.
- Replace CLI inventory-based repack byte estimates with operation-owned
  accounting, including hard-limit admission roll-ups, reused artifacts and
  lost-CAS work. Preserve the distinction between pack bodies and transport.
- Complete the format-removal, Xet/LFS, failure/concurrency, GC/history,
  replica/tiering/mount/browser, CI/platform, hosted-provider, and paired-v1
  release gates. This Git-only workload does not retire any of those gates
  or authorize retiring v1.

### 2.5.59 September 26 overlapping cold-clone pack inventory

A read-only index probe of the completed replay's unchanged root isolated the
regeneration cause. Its three physical packs contain 1,467,547, 174,661 and
19,305 entries. The latter two repeat 1,568 and 58 earlier OIDs, respectively.
Their unique union is exactly the clone's 1,659,887 selected objects: no
missing or extra objects, and no external delta bases. Just 1,626 duplicate
entries made the old exact-once inventory check reject structural assembly
and run native selected-pack generation across the complete repository.

The response producer now proves equality of the unique source OID union and
the authorized selection. The shared Git assembler emits the first occurrence
of each OID without inflating or recompressing its zlib payload. For a source
that loses entries, OFS deltas become OID-based REF deltas; independently
self-contained sources prevent representation selection from creating
cross-source delta cycles. An intact source retains its original headers and
compressed bytes because every entry moves by the same offset. Source hashes,
index/trailer identity, header/index counts, entry CRCs (including discarded
entries), exact selection, response budgets and native receiver validation
remain enforced. Unproven closure or a different object selection still uses
the existing native generation path; this is not permission to return extras.

The regression first failed on four output entries instead of three, then
passed for native Git OFS and REF fixtures. It now also proves that a retained
delta resolves through a removed duplicate's earlier copy, all retained
compressed payloads are byte-identical, and a wholly retained source keeps its
original pack bytes. The last assertion separately reproduced unnecessary
OFS-to-REF header rewriting before its fix. Negative coverage rejects invalid
local base references and a bad CRC even in an entry being discarded. The 19
focused Git repack tests, 28 response-pack unit tests, and 22 response-pack
integration tests pass. The latter cover subset authorization, shallow and thin
responses, cancellation, response budgets, and corrupt artifacts/sidecars.

The first release candidate, SHA-256
`54988291b53a23289c258124c5bc08488a1b231de81ed5a38cd26198976a27f6`,
was tested against the completed 5,000-push root in
`overlap-clone-20260926-r3`. Only its 305-byte derived response-cache descriptor
was removed, after a recoverable local backup; the old response artifact and
all repository data remained untouched. Raw requests prove descriptor misses
and a new response artifact. Independent cold and warm clones pass exact-tip,
native strict full fsck, and 32 source-blob comparisons each; the root bytes
are unchanged. Neither clone invokes `pack-objects` in Trace2. This is clone
qualification on the completed replay, not another 5,000-push run on this
binary.

The intact-source refinement also passed the same cold-miss setup and all
clone correctness checks in `overlap-clone-20260926-r4`, with release SHA-256
`08f37787f8e692efac4b941de141a10626cc3ca763359635798a1611e59e923a`.
Both native strict full fsck runs pass (99.318 s cold and 103.483 s warm),
both sets of 32 sampled blobs match the source, neither response regenerates
packs with native Git, and the repository root remains unchanged. No build
ran during either measured clone operation; unrelated host activity still
prevents a controlled performance claim.

| Measurement | Previous full replay | First structural union | Intact-source refinement |
|---|---:|---:|---:|
| Cold clone | 141.781 s | 98.660 s | 83.247 s |
| Warm clone | 64.190 s | 64.218 s | 66.766 s |
| Cold requests | 194 | 199 | 196 |
| Cold request-body bytes | 1,227,086,068 | 1,258,868,564 | 1,258,663,644 |
| Cold peak process-tree RSS | 2,121,072,640 bytes | 1,814,052,864 bytes | 1,821,720,576 bytes |

Removing native pack regeneration does not make cold clones fast enough.
Both structural candidates increase response-upload bytes by about 2.6% and
use 151 multipart part PUTs instead of 147; they still publish a generated
response before completing the native fetch. Receiver `index-pack` remains on
the critical path, and its wall time includes waiting for that response. The
warm measurement is not an improvement. Preserving intact headers is a
byte-layout guarantee, not proof that it caused the second timing difference.
These timings are observations, not a controlled v1 comparison or a throughput
guarantee. Multi-pack/index reuse, filter/shallow-safe transport selection,
foreground request amplification, the ten-request fetch gate and the remaining
release matrix remain open.

The approved behavioral replacements were rerun with the final source:
`cargo test -p crab-remote --features publication --test checkpoint
incremental_install_enforces_byte_budget_before_pack_payload_reads` passes one
test, and the S3 gateway capsule filter passes five tests. The initial remote
invocation omitted `publication` and ran zero tests; it is not evidence.
Strict all-target Clippy for `crab-git`/`crab-remote-git` still fails at two
unchanged-in-this-edit sites in `crates/crab-git/src/pack.rs`: the nested
reverse-index cleanup condition and the nine-argument private installer.
Those implementations are absent from freshly fetched `origin/main`
`de0bb234abc`, so these are remaining branch gates, not a claimed main-branch
failure. No lint rule, expected result or performance threshold was relaxed.

### 2.5.60 September 26 filtered classic-fetch correctness and lint gates

Native Git against an owned one-pack RustFS fixture reproduced a correctness
failure: `clone --filter=blob:none --no-checkout` received `ok` for its filter,
then exited 128 with `filtered fetch requires protocol v2`. Capability probing
had selected classic fetch before the filter was known. Git 2.50.1's
[`transport-helper.c`](https://github.com/git/git/blob/v2.50.1/transport-helper.c)
confirms that capabilities precede option negotiation, and `fetch_refs` can
send the filter after trying connection takeover. An absent filter during
capability discovery is therefore not a safe full-clone proof.

Classic capsule fetch now retains the canonical filter AST and shares the
existing shallow planner/install path. Full, unconstrained one-pack clones
retain index reuse; filtered requests get the exact authorized planned pack
and its promisor marker, never a silently complete pack. Unsupported filters
clear prior parsed state and fail before fetch I/O. The public legacy
`FetchOptions::filter` refusal remains because that API exists in tag v1.2.3;
it is not the parsed wire-option state. Footer-only views load the missing
visibility metadata under the same root and reject changed refs, peeled refs,
or transaction positions. A concurrent ref change may require retry rather
than mixing the advertised view with newer visibility.

The regression first failed at the real helper fetch-dispatch boundary, then
passed for filtered full and depth-one histories. It verifies omission of the
blob, preservation of commit/shallow boundaries and promisor markers, and
byte-identical recovery in a new helper session. Existing shallow/deepen,
excluded-ref, and timestamp tests also pass: four `classic_capsule_` tests.
The `filter` slice passes 171 tests and the `promisor` slice passes eight,
including hidden-object rejection, unsupported-filter refusal, marker
idempotence, and rollback of only the rejected pack's marker. The shared
rollback owner now removes `.promisor` with the owned pack/index/reverse-index;
unrelated packs and retry idempotence are covered.

Installer and storage lint failures from the previous section are fixed without
changing their integrity checks or public signatures. A private installer enum
keeps verified body identity attached to sidecar-verification authority;
corrupt index checksums remain rejected with and without a verified body.
Metadata's canonical ordinal dictionary/remap/closures now travel in a named
crate-private result, with unchanged wire encoding. Seventeen visibility tests
and ten layered codec tests pass. Obsolete lint expectations and a redundant
import were removed; no lint rule or threshold changed.

Strict all-target Clippy passes for `crab-git`, `crab-metadata`, and
`crab-storage`. Formatting and `git diff --check` pass. The approved
byte-budget integration test (with `publication` enabled) and all five capsule
S3 tests were rerun on this source and pass; their checks were not weakened.

Strict all-target Clippy for `crab-git`/`crab-remote-git` advances to three
remaining remote-Git errors: the selected-member tuple return and the two
oversized snapshot constructors. Minimal-feature CLI builds also emit warnings
outside this change. Full CI and the broader release matrix are not green or
qualified by these focused checks.

Release build SHA-256
`828d9fbcee4d2a81293c8d1880679416d3a58cc3ed594d276841bfea5f104b01`
passes native Git/RustFS verification in
`capsule-filter-routing-2721-IAAfaP/candidate-r1`. The five cases are unfiltered,
`blob:none`, `tree:0`, `blob:limit=1`, and `--depth=1 --filter=blob:none`, all
with `--no-checkout`. Retained protocol traces confirm classic fetch, including
the repeated filter option after ref advertisement. Before lazy reads, native
object enumeration finds three objects for the full clone, two for the blob
filters, and only the commit for `tree:0`; filtered clones have promisor
markers. Each case has the exact source tip, passes strict full Git fsck both
before and after lazy retrieval, and returns the source file byte-for-byte.
The binary hash is unchanged across the run. These small-fixture clones take
0.207–0.348 s, but that is correctness evidence, not a large-repository speed
claim. The completed 5,000 replay and K8s clone numbers in previous sections
remain evidence from earlier binaries, not this candidate.

### 2.5.61 September 26 direct multi-pack clone admission

Classic helper cold clones now reuse multiple stable pack/index/reverse-index
members, not just one. The installer stages and authenticates the entire set
before installing any member, compares the downloaded sorted/deduplicated OID
union with the checkpoint footer commitment, and requires every captured ref
and peeled tip. Identical packs in separate sources are verified independently
but installed once. The returned installation result carries the proof; the
helper no longer predicts successful admission from metadata before I/O.

Regressions were observed before fixing the source: checkpoint-only candidates
could omit a newer per-ref frontier, overlapping cold-clone admission was
single-pack-only, and classic full fetch copied a hidden annotated tag object.
The first case now installs the complete view through the existing frontier
path; configured hidden refs use the same authorized planner as filtered and
shallow requests. Cold body and sidecar bytes share a pre-I/O budget. A corrupt
late source leaves no installed pack. Physical packs retaining extra objects
do not receive an exact-closure connectivity proof.

Git’s keep-file contract requires one index containing every requested tip.
The helper selects that index among the verified installed packs; when tips
span packs, it creates only the existing tip-only proof pack. The standard
protocol-v2 wire continues to require one pack response. No wire format or
stored checkpoint encoding changed. Classic full, constrained and raw-object
fetches now acquire their shared reader-admission ticket at the common dispatch
boundary. Direct installation cannot bypass the repository reader limit, and
source fan-out does not acquire one ticket per member. This restores the
documented admission contract; its coordination requests are included in the
measured request count.

The approved byte-budget and layered-checkpoint/ref-preservation assertions
remain behavioral checks. The follow-up removes all five reader/remote-Git
strict-Clippy failures without suppressions: one `SnapshotLookupSources` replaces
the unshipped constructor stack, maintenance/fetch selection carries its
admission together, and payload windows stay paired with their fetched bytes.
Snapshot/catalog-tail entry points, cache identity, source authentication, and
stored encodings are unchanged.

A new retry regression first failed with `layered member has no payload window`:
an entirely local selection passed integrity/admission checks but then entered
the path that requires downloaded sidecars. The admitted path now skips local
members even when no downloads remain. The test proves zero-origin-read retry,
byte-identical Git blobs, and rejection of same-size local pack/index/reverse-index
corruption. Current focused proof: 17 checkpoint integrations, 22 repository,
29 remote-reader, 28 remote-pack, 30 capsule-reader, and five capsule S3 tests;
strict Clippy passes for both read crates with all targets.

The HTTP consumer check exposed a second regression: the cold installer retained
a nested source/member iterator across I/O, so its future did not satisfy
Tokio's `Send` bound. The checkpoint fixture now runs installation inside
`tokio::spawn`; it reproduced the lifetime error before the fix. Collecting the
member references before suspension preserves order and byte authentication,
and restores the server's existing task boundary without relaxing its `Send` bound.
The spawned checkpoint tests, HTTP compile check, and three integrity-scrub tests
pass, including dependency loss and lease-loss cancellation. The CLI minimal-feature
compile check also passes, with 18 warnings in untouched feature-disabled paths.

The r5 qualification also passed 35 CLI pack tests, six classic fetch tests,
the one-pack wire test, fetch-batch tests and eight promisor tests. The classic
admission regression verifies one released reader slot on both successful and
rejected fetches. No lint or performance gate was relaxed; full CI,
final-candidate replay and the broader release matrix remain open.

Release SHA-256
`9fec45a18be45de7c07cc8efbf6fa18bfa7d39ca95dbf133d382780c615feb9f`
completed `overlap-clone-20260926-r5` against the unchanged r2 repository after
its 5,000 pushes. Both runs used fresh Git destinations; cold/warm denotes
client-cache state, not flushed OS/Docker caches. Comparison with r4 under the
same request-metering harness:

| Clone | r4 wall time / requests | r5 wall time / requests | r5 peak sampled process-tree RSS |
| --- | ---: | ---: | ---: |
| Cold | 83.247 s / 196 | 24.312 s / 18 | 463,601,664 bytes |
| Warm | 66.766 s / 30 | 17.770 s / 15 | 535,412,736 bytes |

Both preserve the three canonical pack identities, return the exact source tip
`6384b87ed0bef8bc893d2d4fd7ab93a1ce0fc2e1`, pass separate strict full Git fsck
(104.149 s and 102.198 s), and match 32 sampled source blobs byte-for-byte.
Git releases the connectivity keep file. Trace2 checks both `start` and
`child_start` events: neither clone invokes `pack-objects` or `index-pack`.
Raw request logs show zero generated-pack cache requests and only 244/159
uploaded bytes for reader coordination. Both download approximately 1.313 GB
of authenticated packs, sidecars and controls. Repository root and binary hash
remain unchanged throughout qualification. The same binary also passes all
five native filter/lazy-recovery cases in
`capsule-filter-routing-2721-IAAfaP/candidate-multipack-r1`.

This is a substantial clone improvement, not the few-second target or a v1
parity claim. The largest payload GET takes 13.821 s cold and 8.156 s warm
through the metering proxy; a direct-endpoint control is still needed to
separate provider/transfer cost from instrumentation overhead. No new 5,000-push
replay ran on this binary. These live measurements also predate the lookup-source
cleanup, local-retry fix, and spawned-installer fix. Incremental-fetch request-count,
foreground-compaction amplification, broader lint/CI and complete lifecycle/provider
gates remain open.
v1 retirement is not justified by these results.

### 2.5.62 September 26 updated-candidate full replay

Run `candidate-layered-20260926-r6` started at `2026-09-26T10:59:05Z`
against a verified-empty prefix in the owned Docker RustFS instance. It uses
the unchanged read-only Kubernetes source revision from r2 and release SHA-256
`871edd4e873552772bf10e05914c2233b67e22b0642ccd59a4c4e58dbd0b6dce`.
The workload remains a seed push, 5,000 individual first-parent pushes, fetch
and repack every 500, and independent final cold/warm clones with native and
Crab fsck and sampled blob comparison. No gate was relaxed. The harness links
the selected external binary instead of copying/installing one and rejects a
changed binary hash at completion; all 18 harness tests pass.

Seed measurements: seed push 181.448 s / nine requests, seed
maintenance 6.176 s / nineteen requests, and initial clone 14.229 s / thirteen
requests. Native strict full Git fsck and remote Crab fsck both passed for the
seed; the latter took 115.881 s / seventeen requests.

The first 500 pushes averaged 301.418 ms, with p95 582 ms, p99 848 ms, and
7.012 requests per push. Their incremental fetch passed tip/connectivity and
pack-preservation checks, installed one new pack, and took 5.034 s / 238 requests.
Its 230 capsule requests are all distinct source/range pairs across 24 physical
capsules. The unchanged ten-request gate therefore still fails: repeated-read
caching alone cannot remove these compulsory reads. Interval maintenance took
12.893 s / 94 requests and retained two packs. The proxy measured 269,799,447
response bytes; the CLI's inventory-based pack-byte estimate is still not actual
transport accounting.

Offline range-union analysis of the retained request log isolates two separate
amplifiers. Fetch 500 requested 74,970,965 capsule bytes but only 64,966,799
distinct bytes; fetch 1,000 requested 46,869,908 but only 36,850,381 distinct
bytes. Neither has an exact duplicate source/range pair, but both reread about
10 MB through overlapping ranges. One 32,138,088-byte run accounts for 78
requests at fetch 500: footer, admission sidecar, 75 index windows, and a broad
packed-entry window covering most of those index windows. Twenty small sources
each require four ranges. Source inspection matches this sequence:
`load_capsule_run_control` loads footer and admission separately,
`plan_pack_index_reads` bounds index-window gaps, and the packed-entry reader
coalesces bodies independently. A bounded shared source read is therefore a
candidate to measure; merely caching identical ranges cannot remove this cost.

Physical source count is an independent floor. The current 32-leaf compaction
wave leaves 24 sources after 500 pushes. Eight other operations at the first
fetch cover root/ref capture, two namespace listings, replica discovery,
checkpoint control, and the reader-admission acquire/release pair. Even one
read per source would still exceed the ten-operation gate. An offline replay
of the current compaction algorithm reproduces the measured 7.012 average
push requests; changing fan-in to four predicts 7.488 requests per push and
six remaining sources, before retries, contention or large-file traffic.
This is a request-count model, not a performance result: lower fan-in still
cannot meet the fetch target with the existing control path and increases
compaction frequency. No fan-in, integrity check, or qualification threshold
was changed on that basis.

The complete run finished at `2026-09-26T11:44:47Z`; the binary hash remained
unchanged. All 5,000 individual pushes, ten incremental fetch/repack intervals,
seed checks, cold/warm native full Git fsck, final remote Crab fsck, exact tips,
and 32 sampled Git blobs per clone passed. Final remote fsck took 157.346 s /
293 requests and read 4,251,756,555 bytes, including retained history. Every
incremental fetch preserved existing local packs and added exactly one pack;
Trace2 recorded no incremental Git repack. Across all ten fetches there were
zero requests to the seed capsule or standalone pack layers.

| Completed measurement | r6 result |
| --- | ---: |
| Incremental push mean / p50 / p95 / p99 | 290.56 / 249 / 558 / 944 ms |
| Incremental push maximum | 2,362 ms |
| Origin requests per push, mean / maximum | 7.012 / 42 |
| Incremental fetch mean / p95 | 7.185 / 17.367 s |
| Origin requests per fetch, mean / p95 | 249.9 / 314 |
| Cold final clone | 28.784 s / 18 requests |
| Warm final clone | 30.265 s / 18 requests |
| Interval repack wall time | 12.893–29.539 s |
| Pack count after interval repack | 2–3 |

All ten 500-push windows averaged 7.012 requests. Their mean latencies ranged
from 261.09 to 331.88 ms; several window p99s exceeded one second. There were
4,850 ordinary six-request pushes averaging 279.24 ms and 150 compaction
pushes averaging 656.49 ms. Mean push latency excludes periodic fetch/repack
and integrity-check time. Both clones installed the same three canonical
packs without `pack-objects` or `index-pack`, transferred about 1.313 GB, and
used 478,674,944 / 469,336,064 bytes of peak sampled process-tree RSS.
Warm denotes client-cache reuse, not flushed OS/Docker caches.

The harness deliberately returned nonzero with `qualification performance
gates failed`: both the ten-request and ten-second incremental-fetch p95 gates
failed. The slow 4,500-commit fetch spent about 13.61 s before Git started
`index-pack`; indexing then overlapped response delivery and took 2.50 s.
The cold clone's largest payload GET took 17.275 s through the metering proxy.
Unrelated VM/compiler activity was observed on this shared host, so these are
not controlled v1/v2 regression estimates; that does not erase either failed
gate. The 18 replay-harness and 19 proxy tests passed again. Full CI, paired
v1, complete Xet/LFS/product/provider qualification, and hard-cutover cleanup
are still required. This is a completed correctness replay, not production
qualification.

After the replay terminated, `direct-control-20260926-r6` cloned the same
final remote through `http://127.0.0.1:19124`, bypassing the request proxy,
with a fresh destination and client cache and the unchanged r6 binary.
The CLI reported 26.018 s overall, 17.001 s in `pack_fetch`, and 6.936 s in
checkout; `/usr/bin/time` measured 26.03 s wall time. Native strict full Git
fsck passed separately in 101.44 s, the tip matched the source, and all 32
sampled blob digests matched. Request count was not metered in this control.
The host and OS/Docker caches were not isolated or flushed, so this single
sample does not precisely estimate proxy overhead. It does rule out treating
the proxy as sufficient explanation for the observed tens-of-seconds clone:
the direct clone was not a few-second operation either.

### 2.5.63 Post-replay bounded index matching

The canonical remote-Git batch reader no longer searches every requested OID
against every small frontier index. It sorts the request positions once, probes
the smaller of that set and each verified index, and lazily removes resolved
self-contained entries before a larger index. Original order, duplicate requests,
missing objects and external-delta preference remain unchanged. Git index order,
offsets, CRCs and checksums still come from `parse_pack_index`; neither source
selection nor object-store admission changes. Snapshot, catalog-miss and capsule
callers share this implementation; no format or public API was added.

A deterministic comparison-count regression failed with 12,582,912 ordering
comparisons in the extracted old matching loop for 8,192 requests across 256
small indexes. The new loop stays below 262,144; the inverse point-read case
also stays bounded instead of scanning a large index. These are algorithmic
fixture counts, not a measured wall-time speedup or a request-count reduction.
Property checks preserve duplicates, misses and caller positions; focused tests
retain corrupt-data rejection, cancellation, delta selection and independent
byte/request budgets.

The approved checkpoint test verifies byte-budget rejection before pack-body
reads, absence of partial installs, and byte-identical native-Git reads on a
sufficient-budget retry. S3 tests verify layered-source, ref and transaction
preservation plus a subsequent ref advance, not the removed embedded-checkpoint
shape. The r6 release binary remains unchanged; the full replay measurements
above predate this CPU-only change and must not be presented as its qualification.

Final-source proof: 33 reader tests, 28 pack tests, 26 native-Git repository
tests (pack and canonical-snapshot filters), 17 checkpoint integration tests,
and five capsule S3 tests pass: 109 focused tests. Strict all-target Clippy for
`crab-remote-git`, package formatting and `git diff --check` pass. This does
not replace the outstanding full replay, paired v1 comparison or CI gates.

### 2.5.64 Compacted-run lookup-index locality

The r6 request trace exposed a second cost beyond index matching: indexes were
interleaved with complete capsule pack bodies. Its first 500-commit fetch made
230 capsule requests across 24 physical sources. One 32 MB compacted source
required 75 index windows. These were not identical repeated ranges, so a
duplicate-read cache would not remove the fragmentation.

`CRBRUN05` appends a contiguous copy of the indexes in each compacted run.
Original capsules, pack identities and canonical member ranges are unchanged;
installation, recovery and suffix repack still consume those original ranges.
The captured frontier supplies the copied ranges only to the shared lazy index
reader. Each range retains its canonical index hash, and the reader preserves
Git checksum/inventory validation, request/byte admission and cancellation.
Full-run decoding additionally verifies the aggregate pool and each copy.
Malformed, missing or mis-sized pool descriptors fail closed. Leaves do not
duplicate indexes, and merging rebuilds the pool rather than nesting old pools.

The reader-boundary regression publishes a checkpoint followed by 32 updates
with distinct incompressible 128 KiB pack bodies. The canonical-index control
failed with 32 origin reads; the pooled path passes with one read and exactly
the sum of index bytes. The result must contain all 32 requested objects before
their identities and bytes are compared; a shortened result cannot pass via
iterator truncation.
Fresh-runtime checks reject a one-byte-short budget and corruption in the last
pooled index without caching any partial parsed-index result. These are focused
fixtures, not end-to-end Git fetch latency or authorization qualification.

The tradeoff is extra index bytes, copying and hashing during compaction. The
physical source count and control/admission requests remain unchanged; this
alone cannot meet the ten-request fetch gate. It does not justify weakening
that gate, skipping publication checks or claiming v1 parity. The previous r6
measurements predate this format and the index-matching change. New qualification
must use a new remote prefix and one immutable candidate binary throughout.

Focused proof: 62 metadata capsule tests, 30 reader-orchestration capsule tests,
20 writer capsule tests, 33 shared-reader tests, 18 checkpoint integration tests,
five capsule S3 tests and nine HTTP maintenance tests pass (177 total). The
approved byte-budget and layered-checkpoint/ref assertions are included; no
performance threshold was relaxed. Strict all-target Clippy for `crab-remote`
with `publication` passes.
After the replay, all 18 checkpoint integration tests and five capsule S3
tests passed again, including the strengthened 32-object cardinality check.
Package formatting and `git diff --check` also passed.
The completed replay below does not satisfy the wider release gates.

The minimal-feature release build passed. A new 5,000-commit RustFS replay,
`candidate-pooled-indexes-20260926-r7`, ran from 12:33:14 to 13:16:32 UTC on
2026-09-26 with unchanged binary SHA-256
`36789d5a86f98a6f16f8a15b2fc2c865a61b3d9994dc98e918d68f8de32a2b9d`.
It used the same pinned Kubernetes input and unchanged gates, fetching before
each 500-commit repack. All 5,000 pushes, ten fetch/repack intervals, seed/final
Git and Crab fsck, exact final tips, and 32 sampled blob comparisons in each
cold/warm clone passed. Every incremental fetch retained prior local packs and
installed exactly one new pack. The request log records no seed-capsule or
standalone-layer reads during those fetches, and Trace2 records no fetch repack.
Git invoked auto-maintenance checks; those are not repack events.

The host was an arm64 Mac14,13 with 12 logical CPUs and 32 GiB RAM, macOS
26.5.2; Docker had six CPUs and about 7.67 GiB RAM. Another worktree was
compiling at startup; unrelated test, VM and indexing activity was observed
later. No competing process was stopped. Host isolation and a paired v1
benchmark remain missing proof; timing differences from r6 are not controlled
regression estimates.

Push mean/p50/p95/p99 were 264/217/517/816 ms, with a 7,740 ms maximum.
Request count was 35,060 total, 7.012 mean, six p50/p95, forty p99, and
forty-two maximum. Each 500-push window averaged exactly 7.012 operations;
window mean latency ranged from 231.91 to 308.05 ms. The last two windows
were slower, and the 4,001–4,500 window had 1,018 ms p99: these results do
not establish a sub-second bound or flat tail latency.

Seed push completed in 205.157 seconds / nine requests, checkpointing in
5.946 seconds / nineteen requests, and initial clone in 13.586 seconds /
thirteen requests. Exact seed tip, native strict full Git fsck and remote Crab
fsck passed; remote fsck took 111.954 seconds / seventeen requests.

The first 500 incremental pushes averaged 270.64 ms and 7.012 origin
operations. Their fetch passed tip/connectivity and existing-pack preservation,
installed one new pack, and took 4.783 seconds / 104 requests versus r6's
5.034 seconds / 238. Response bytes fell from 75,017,415 to 65,997,857.
The retained trace now has exactly four requests for each of the 24 capsule
sources, plus eight control operations: pooled indexes removed the fragmented
index windows, but source fan-out and separate control/admission/body reads
still fail the ten-request gate. Git `index-pack` took 1.801 seconds overlapping
response delivery; subsequent connectivity traversal took 0.488 seconds.

Over those 500 pushes, observed upload/download bytes changed from
213,426,026/350,064,579 in r6 to 215,590,981/353,403,384 in r7. This captures
the compaction tradeoff without assuming identical pack encoding or an isolated
timing comparison. The first interval repack took 12.340 seconds / 94 requests
and retained two packs.

All ten completed interval fetches preserve the expected tip and prior local
packs, with one newly installed pack each:

| Interval | Fetch time | Origin operations |
| --- | ---: | ---: |
| 500 | 4.783 s | 104 |
| 1,000 | 2.404 s | 122 |
| 1,500 | 3.896 s | 107 |
| 2,000 | 4.680 s | 121 |
| 2,500 | 5.181 s | 116 |
| 3,000 | 4.682 s | 104 |
| 3,500 | 4.855 s | 109 |
| 4,000 | 13.076 s | 109 |
| 4,500 | 12.433 s | 108 |
| 5,000 | 6.497 s | 104 |

Fetch mean was 6.249 seconds; p95 was 13.076 seconds. Mean requests fell from
r6's 249.9 to 110.4 (1,104 total), and total response bytes from 700,618,367
to 598,707,877. The unchanged ten-request and ten-second p95 gates both fail;
the harness returned nonzero only after completing the correctness checks.
The 4,000-commit trace has about 8.245 seconds before `index-pack`, 1.311
seconds indexing overlapping response delivery, and 3.460 seconds in the
subsequent connectivity walk. Its recorded request durations sum to 1.795
seconds; object-store latency alone does not explain that sample.

Final cold/warm clones took 32.534/34.305 seconds and 15/18 requests, each
downloading about 1.313 GB. Warm denotes client-cache reuse, not flushed
OS/Docker caches. The cold clone's largest payload GET took 16.723 seconds;
neither clone meets the few-second goal. Final remote Crab fsck passed in
179.014 seconds with 295 requests and 4,260,742,108 response bytes including
retained history. Full CI, Xet/LFS and product/provider qualification, paired
v1, accurate repack I/O telemetry, and hard-cutover cleanup remain required.

The 1,000-commit fetch still has 24 physical sources and eight non-capsule
operations, but 114 capsule reads. Several index ranges repeat after body
reads, and some body windows are split. The shared reader's 256-entry parsed
index cache is a plausible contributor with a 500-member frontier; the trace
does not by itself prove eviction versus a later lookup phase. Reproducing
that path under cache pressure is separate from the larger source/control
fan-out problem. No cache limit or correctness check has been relaxed.

### 2.5.65 Bounded per-ref capsule-window rollup

The retained 5,000-push trace showed a 24-source frontier at each
500-commit fetch boundary. The existing 32-run batching reduced interim
publication cost but left too many immutable sources for the unchanged
ten-request fetch gate.

CRBRUN07 changes run levels from exact powers of two to authenticated size
classes: `level = ceil(log2(capsule_count))`, with the exact count still stored
and capped at 512. Root, history, and ref-head contracts move together to
versions 4, 3, and 5. Readers reject CRBRUN06 and older layouts; this is an
unshipped hard cutover, not a compatibility reader.

After every 500 capsules since the prior rollup, the per-ref writer reads the
selected suffix and publishes one immutable run. Older completed rollups retain
their identities. The ordinary 32-run batching remains for sub-window writes;
all capsule bytes, transaction ordering, pooled-index checks, and object/member
admission are preserved. Xorbs and shards remain external and are not copied
into the run. The two-window regression proves one run at commit 500, two runs
at commit 1,000, exact transaction order, unchanged first-run identity, and
average observed in-memory store operations below ten per push.

Metadata (236), reader (219), and writer (31) unit tests pass. This is focused
in-memory proof only: boundary push tail latency, write amplification, request
counts against RustFS, the 5,000-push workload, fetch latency, and the 100 GiB
Xet workload must be measured on the final immutable binary before qualification.

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
`CRBRUN05`, which preserves the complete capsule bytes, adds one aggregate,
authenticated pack-member directory, carries the transaction plus small
visibility/catalog controls in the run footer, and appends an authenticated
sorted OID-to-member admission sidecar. A control section larger than
512 KiB remains committed by its capsule range/hash but is detached from the
footer; the bounded reader fetches that exact range, verifies its BLAKE3, and
then materializes the control. This keeps a production-sized initial
visibility proof from turning the run footer into a hot multi-megabyte object
without weakening authorization. Current publication includes the authenticated
suffix offset and hash in the pointer, avoiding trailer discovery. The reader
opens that suffix and separately loads the committed admission sidecar, then
addresses nested `CRBCAPS2` pack bodies and sidecars without fetching the whole
capsule payload. Old development pointers still use trailer discovery; removing
that normal-path compatibility is part of the hard-cutover work, not a reason
to attribute its extra request to current writers. A standalone source binds
one member in a `CRBPKL01` object.
In both cases the reusable local Git pack filename is derived from the
pack-body content hash, not the containing source identity.

A nonempty compacted run also carries an index-copy pool between its capsule
bodies and admission sidecar. Index bytes are concatenated in member order;
the footer commits the pool range/hash, and each derived slice retains the
canonical member index's length and BLAKE3. Full decoding verifies both the
pool hash and every copied index. Control decoding requires the exact pool
length and contiguous placement; ordinary index reads still verify each slice,
Git checksum and inventory before caching. A leaf has no pool. This adds no
object-store object or publication request, but increases compaction bytes and
hashing work by the copied index bytes.

The pool is not substituted into canonical `PackMemberDescriptor` ranges:
doing that would break member ordering and make whole-member reads span other
pack bodies. Only the shared reader's captured frontier lookup sources use the
pooled indexes. Install, cold clone, repack, recovery, stable-source handling,
xorbs and shards retain their original byte ranges and proofs. Existing range,
gap, overread, member-count and aggregate read budgets remain unchanged.
Readers/writers must deploy together; `CRBRUN04` is an unshipped development
format and is rejected by the new run decoder. Prior qualification prefixes
are retained as historical evidence, not rewritten in place.

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

The checkpoint footer also carries a bounded recent suffix of the authenticated
per-ref visibility transition history. Ordinary control-only fetches can use
that suffix to plan a have-to-tip delta without downloading the large ordinal
visibility body. The complete history remains in the body; an older or
incomplete have chain deliberately falls back to the existing authenticated
catalog/traversal planner, so this acceleration hint cannot weaken correctness.

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
uses the authenticated `CRBRUN07` control suffix, its embedded control bundle,
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
visibility dictionary. Post-checkpoint frontier runs also carry an exact
physical OID-to-member directory. The current repository builder passes only
visibility-added OIDs from that directory as lookup hints, leaving physically
present, unselected delta bases to a broad preferred-index probe. That gap must
be closed without turning physical membership into visibility authorization.
Delta-base resolution may still consult older controls through the same bounded
cache when a dependency genuinely lives outside the frontier.

That is the target read contract. `CRBCKP05` authenticates the decision with
the compact ordinal proof and control-only frontier. Stable sidecar admission
and selected-range coalescing are live. A normal frontier hit uses the
authenticated OID-to-member admission directly; when a ref update reuses an
object from an older stable source, the reader performs one batched locator
join, then re-runs the same sidecar/object-set and external-delta proof before
installing the selected immutable members. The join is therefore a bounded
miss path, not a scan of every source, and there is no extra lookup on the
normal frontier-admitted path. This does not establish a low request count:
the r6 frontier-only incremental fetches averaged 249.9 requests, despite
avoiding stable pack bodies. Fragmented index ranges, overlapping index/body
reads, physical source count, response construction and local Git work all
remain performance concerns. Batch index matching now probes the smaller
side instead of scanning every requested OID per small member; that CPU-only
change does not remove the separate source-read amplification.

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

The canonical `CRBCKP05` reader keeps checkpoint sources range-addressable,
includes the visible post-checkpoint capsule frontier, and lets the remote
reader select only missing response objects. The unshipped `CRBCKP03` reader
has been removed; this hard cutover does not retire the separately supported
v1 protocol. A changed checkpoint identity must not force every active source
into the local Git object database. Git's own later maintenance remains valid,
but it is no longer forced by the layered checkpoint path.

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

Git upload-pack still emits one response pack, not a sequence of standalone
layer files. The read module selects wants minus proven common haves and
resolves selected objects and delta bases through the merged locator. Verified
complete, disjoint members may be structurally concatenated with one response
header/checksum and preserved in-member delta distances; otherwise selected
entries form one valid self-contained or negotiated thin response pack.
For an ordinary unfiltered fetch of exact visible tips, an authenticated
transition chain to a client have can end wire negotiation immediately with
`ready`. A historical have need not still be visible, so the server sends no
individual ACK for it. The complete authorized, byte-bounded response plan
still runs before pack output; absent or ambiguous chains keep negotiating.

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

Read-budget and cancellation policy must also survive transport acceleration.
The signed-range path reproduced a wrapper bypass: both large `range_get`
and the direct cold-clone file downloader could enter URL signing without
running the caller's admission or storage observer. Eligibility now lives at
one storage boundary. Wrapped/routed reads retain the canonical object-store
transport; unwrapped reads retain acceleration and explicit presigning remains
available. This changes no publication requests or integrity requirements.

The same boundary now rejects missing, duplicate, malformed, overflowing, or
wrong-offset `Content-Range` headers before exposing the response body. HTTP
regressions reproduced both file and memory reads accepting invalid ranges
with valid lengths; one shared header check fixes both without extra requests.
Nonzero-offset file extraction and exact pack/sidecar hashes remain covered.

Seven focused regressions, all 216 enabled storage tests, and strict storage
Clippy pass. A separate RustFS 1.0.0 GA test verifies the actual signed file
download and exact bytes, rejected admission before payload delivery, and one
accepted/observed 8 MiB range charged once. This is source-level and live storage
proof. The retained 5,000-push binary predates this hardening. The September 27
native-cache candidate includes it and passes the small installed clone checks
below; a fresh performance replay and full affected-consumer proof remain open.

Warm native-pack reuse passes eight focused cache, transport and real-Git
regressions and the unchanged installed live clone probe. The local
cache owns streaming BLAKE3/length checks, private atomic
publication, capacity reservations, and the `git-pack` maintenance family.
Cache-store owns selected-origin routing; the reader still authenticates
sidecars, indexes, locators and the complete visible union before installing.
No remote-service cache admission or authorization shortcut is introduced.
Cache fills add a verified local copy on cold reads; whether this costs more
than it saves on local RustFS must be measured, not inferred from fewer GETs.
The real-publication fixtures cover original capsule-run and physically
repacked-layer sources, independent cold/warm Git databases, corruption repair,
sidecar rejection, exact blobs, strict Git fsck and pre-I/O byte admission.
Local tests cover capacity and destination failures without evicting healthy
cached bytes. The broader selected source checks passed 221 distinct tests,
strict affected-crate Clippy, formatting and the normal CLI/helper/cache-server
installation. That candidate still failed cancellation; the subsequent fix
and installed proof are recorded in §9.6. Fresh performance qualification
remains open.
These edits are not part of the ongoing v1 baseline's frozen installed binary.
Focused tests now overlap that replay with one low-priority build job; its
timing is not a controlled performance comparison.

Installed candidate SHA-256
`5b4efdc2316179b9a52625e05ba17d674bb180b37dcb2c87a2c3ac191154d5be`
passed all 24 checks in `warm-native-clone-ga-20260927-r3` on RustFS 1.0.0 GA.
Independent cold/warm clones read 535,739/10,630 response bytes, respectively,
with 13 requests each and no repeated warm pack-body GET. Exact tips, both
256 KiB blobs and strict full Git fsck passed. This proves byte reuse, not a
Kubernetes-scale latency improvement. The separate fresh-bucket fault run
passed 57 checks through same-size cache corruption/repair, warm reuse,
origin-sidecar corruption rejection, no installed Git pack on rejection and
conditional source restoration. It then failed the cancellation deadline;
that candidate's overall negative matrix is red, not qualified. The next
installed candidate passes the unchanged warm probe and all 79 fault-matrix
checks, including cancellation, as recorded below. The failed evidence is
retained rather than replaced.

This retention path covers classic native-pack installation and CLI remote
snapshot reads. The protocol-v2 wire's direct one-pack response streams through
`Store::get_stream`; it does not use the file installer and has not gained this
cache. Filtered/shallow selection and strict administrative readers also keep
their existing paths. Cache-service and wire-stream reuse remain separate parity
work, not implied by the classic clone regression fixture.

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

Required contract: every expensive phase observes cancellation before
publication and drains owned work before releasing its resources. The cold
installer regression below has source and bounded installed-CLI proof; this
does not qualify arbitrary abandonment or every administrative path. Temporary
local state may be discarded after draining. Uploaded immutable objects are harmless until
named. Once root CAS succeeds, all named objects were already verified and
durable. Lock and GC-fence release rules remain unchanged.

The external-base path in `crates/crab-remote/src/checkpoint.rs` creates a
private `RemoteGitRuntime` in `read_layered_delta_bases`. Its owner now retains
the read result, finishes or drops the operation context, and awaits runtime
shutdown before returning that result, including open/read failures and
cooperative cancellation. This applies the runtime's separate cancel-and-drain
contract; it is contract hardening, not a reproduced leak fix. Arbitrary abort
of the enclosing future remains a separate lifecycle qualification gap. The
ordinary Kubernetes replay does not prove this external-base path safe.

A separate installed-CLI cancellation probe now exposes a cold-clone gap in
the retained GA-replay binary (`62ee1929ede2154d3d54e36f7d7975b49d4aab1ac7eaf1716b8f470c876932f6`).
The meter buffers one real fixture layer response, then a single SIGINT is
sent to the selected remote-helper descendant. Both September 27 r3/r4 attempts
remained blocked beyond the unchanged ten-second probe deadline and required
forced cleanup. These are failed cancellation checks, not performance samples
or new-cache qualification. That native installer did not receive the caller's
cancellation token; the admission owner awaits the operation before releasing
its slot. The installed native-cache candidate also failed the same scenario
in r5/r6 and in the fresh cache-fault matrix. r5 took 10,203 ms before forced
cleanup. Enabling only the existing `crab=warn` tracing filter in r6 confirmed
that the helper received SIGINT and cancelled its token while the download
remained stalled. Transport-wrapper hardening alone therefore does not fix
this unwrapped cold-clone path. Cancellation must reach the pack read without
dropping the installation worker or releasing its reader lease prematurely.

The following source fix now propagates the operation token through native
and incremental installation, cache routing, signed extraction and ordinary
range reads. Signed extraction stops response-header/body and backoff waits;
parallel failures cancel siblings but drain every file writer rather than
dropping them. Tokio 1.53.1 requires flushing to finish pending file I/O before
the private directory can safely be removed. Blocking Git installers remain
awaited, and the existing admission owner still explicitly releases its ticket.
The selected storage layout now owns both paths and origin, removing duplicate
store arguments rather than creating another cancellation transport.

The new HTTP regression failed before the fix, then passed cancellation before
headers and during partial bodies for combined and separate source ranges.
Noncontiguous exact-byte/hash reads, sibling failure cleanup and cancellation
during retry backoff also pass. All 219 enabled storage tests, all 32 shared
checkpoint integration tests and strict affected-crate Clippy pass. The native
fixture covers cold/warm cache and original/repacked sources, empty private
staging after cancellation, and an independent exact-byte/strict-fsck retry.
Nine focused CLI tests also pass, covering cancellation before input, reader
admission release, hidden refs, filtered/shallow fetch and promisor authorization;
43 replay/request-meter tests pass. The unchanged push/repack regression still
fails only its final 12-request repack ceiling at 17 requests, after proving the
six-request incremental push, round trip and GC checks. No ceiling was changed.

Normal `make install` completed for the next candidate, SHA-256
`201414e73474fc64e25c2326a5a616575d640e277213c1ecbbacd967306d501c`.
Its frozen Rust/manifests/lockfile fingerprint remained
`de2d8d273ead853295591998849cd0f345e3f7c752d62e25bfed570b6b8df69e`.
On RustFS 1.0.0 GA, `native-clone-cancel-after-cache-ga-20260927-r7`
passes all 20 checks with the original ten-second deadline: the cancelled
command finishes in 258 ms total without forced kill, publishes no pack or
cache entry, removes private staging, and leaves a released reader-lease
tombstone. An independent retry proves exact tip/blob bytes and strict full
Git fsck; origin bytes remain unchanged. Report SHA-256:
`ae1b6e31ac2dc327c11884f4915cfc00dcefdc78051064c6b2b5a9109b6b3686`.

The unchanged warm probe `warm-native-clone-ga-20260927-r4` passes 24 checks:
cold/warm response bytes are 535,739/10,630 at 13 requests each, with zero
repeated warm pack-body GETs. The fresh-bucket
`native-pack-cache-faults-ga-20260927-r2` passes all 79 checks, including
same-size cache corruption/repair, origin-sidecar rejection and restoration,
lease release, independent cancellation retry and subsequent warm reuse.
Its cancelled command finishes in 395 ms total. Report SHA-256 values are
`92e3c4c7c06623d319ee6511d7b8cfe86dac96ac18f81400d194aaf87c03c11d`
and `643c7b61d3333bc94cf86a6b41d7c35ac3808d823e09ee6ba5a0a9fed390e609`,
respectively. All three private probe hashes are unchanged from their earlier
runs. These 123 checks prove this small-fixture slice, not Kubernetes latency,
cancellation during large local copies, or full protocol/product parity.
Arbitrary future abandonment and administrative entry points without an
operation token remain explicit gaps.

A subsequent wire-path audit found three independent source-download cleanup
failures in `crab-remote-git`, outside the native installer covered above.
The batch returned while both started sibling writers remained unfinished;
the pack stream returned with seventeen queued bytes unflushed after a source
failure; and a missing source escaped pack generation before explicit operation
closure, recording cancellation instead of the real storage failure. Each was
reproduced by a failing regression before its fix.

The shared downloader now cancels only its operation child, drains body and
sidecar futures and skips queued sources. Canonical, embedded and inline pack
bodies use one length/BLAKE3/Git-checksum verifier that flushes on every return
path. Sidecar writers also drain on failure, and generation propagates its
result through `OperationContext::finish`. Wrapped sibling cancellations cannot
replace the source failure. No public API, storage format, request threshold,
or dependency version changes for this fix.

Source fingerprint
`71fbffb2bc581b3c3e29dac1b27b36116723c90d1622a3b7acb00fd852bd78e4`
passes 63 focused reader/pack/close tests, 22 real-Git integration tests and all
39 CLI wire tests; strict all-target remote-Git Clippy, formatting and diff
checks pass. Normal isolated `make install` completed with unchanged source;
the installed binary and helper have SHA-256
`902cec59bf905d6f5072d9f56d7dde7e9d11fbfb1df2e9a30df70c192c39a30b`.
The earlier 123-check installed proof does not include this later source change.
The subsequent installed failure probe, `wire-pack-source-failure-ga-20260927-r2`,
passes 22 checks against the retained Kubernetes GA repository. It admits
1,659,887 objects through actual protocol-v2 with no common haves, injects a
missing source after a sibling body has begun transferring, and exits 168 ms
later without forced termination. The source error reaches explicit `Error`
completion; private temporary files are gone, no Git/generated pack is published,
reader and producer lease tombstones are released, and root/source identities
are unchanged. Report SHA-256 is
`35ba21caf9a4fd636819b99b8d4d44f73ccca7ef85b7f50a7b0c5b069b703c3d`.

The first probe attempt is retained as failed: an empty Git clone selected the
classic native installer and did not exercise wire generation. The corrected
fixture contains one unreachable local blob but no refs, selecting wire transport
without a common commit. Neither the failure deadline nor correctness gates
changed. Independent recovery `wire-pack-source-recovery-ga-20260927-r1` also
passes all 18 checks: a complete wire fetch into a fresh object database returns
the exact Kubernetes tip, passes full strict Git fsck and 32 sampled blob-byte
comparisons, leaves no private temporaries, releases reader/producer leases and
preserves the published root and source identity. It permits only coordination
and derived generated-pack cache writes. Report SHA-256 is
`1d26a49267884c4a6a12df0145c9ecc8649c8aba6744b1b7cdadf2cef01be999`.

This forced wire-path recovery is not a default cold-clone performance pass:
fetch takes 107.147 seconds, followed by a separate 95.532-second strict fsck.
Three source packs produce 1,659,887 selected objects with zero object inflation;
generation takes 31.850 seconds including 11.746 seconds of source download.
The total 189 requests include 151 multipart parts for the derived response-pack
cache. Shared-host timing and the deliberately nonempty destination prevent
comparison with the classic direct-pack cold-clone path. Caller-cancellation
proof and a rebuilt-binary replay remain required. Shared generated-pack producers intentionally outlive
individual waiters; private helper runtime shutdown requires its own live
proof and cannot be inferred from these awaited-download tests.

The subsequent `wire-pack-cancel-ga-20260927-r2` probe reproduced that gap on
binary `902cec59`: a single SIGINT sent only to the helper, after a filtered
wire producer acquired its lease and began reading a pack body, exited in
157 ms but left its producer and producer-reader leases unreleased. There
was no forced kill, temporary-file leak, or Git/cache publication. Report
SHA-256 is
`3d50dc0001c070ed1390c95027a076d6f7910f61b22669a55f501242de47dc07`.
The first attempt targeted a large body read that the selected-entry path did
not perform; it is retained as an invalid-path attempt, not a passing test.

The source correction makes the helper own one runtime across classic and
wire fetch and await its shutdown after every loop exit. Lease-bound producers
and cached-artifact readers receive a child token and drain their work before
lease release, including after renewal failure. Request-bound producer closures
now accept that token explicitly; all workspace callers are updated. This
changes an internal Rust API, not the storage format, lease timing, or request
gates. Both the missing-runtime-shutdown and premature-release regressions
failed before their fixes and pass afterward. The lease test covers cancellation,
preservation of a real source error, and renewal failure at the actual 60-second
interval. All 22 real-Git pack/cache integration tests pass, including shared
producers surviving individual waiter cancellation. The complete focused set
passes 134 tests. Strict all-target remote-Git Clippy passes. The CLI's existing
CI lint command passes with warnings; a separate stricter `-D warnings` probe
fails with 610 crate diagnostics, including a new outer large future that was
subsequently boxed and retested. The strict CLI probe is not claimed green.

Normal private `make install` produced binary SHA-256
`806820af681765230187ad983fb1c5c6deef8808d4f49c5803047e3db43d17d9`.
The frozen Rust/manifests/lock fingerprint remained unchanged across the build.
Installed retry `wire-pack-cancel-ga-20260927-r4` passes all 12 checks: one
helper-only SIGINT exits in 181 ms, all three participating leases are released,
no private files or Git/generated pack survives, and the published root is
unchanged. Report SHA-256 is
`ff437cf2ddb534dea076f4b720eaf7d7bed97847194b527946dc88a44f21eff4`.
The ten-second cancellation deadline and cleanup assertions remain unchanged.

Retry r3 failed before injection because its write allowlist denied the exact
lease-clock object needed to reclaim the old binary's expired producer lease.
The corrected probe permits only that scoped coordination clock in addition to
the lease keys; it still forbids repository and generated-pack publication and
checks lease tombstones separately from clock objects. r3 and the post-exit
proxy connection-reset diagnostic remain retained. This proves the exercised
held-body cancellation case, not every timeout, CPU-work cancellation, or
provider failure.

Fresh `wire-filtered-recovery-ga-20260927-r1` passes 52 checks on that same
installed binary: exact Kubernetes tip, promisor packs, strict native Git fsck,
32 sampled blobs initially absent, explicit promised-object fetch, byte-identical
sample contents, a second strict fsck, released participating leases, no private
temporaries, and unchanged root/source identities. Only scoped coordination and
derived response-cache writes are permitted. Report SHA-256 is
`9b3b1736f8432e383f25a7206095475ce519ef96ed4530da3a7cb4995a2f5a03`.

This is correctness proof, not a filtered-fetch performance pass. The initial
fetch takes 41.607 seconds and 43,723 requests, transferring 5,902,417,918 response
bytes. Selected-entry generation copies 1,027,194 entries and converts 86,461
deltas without materializing entries; its 213,284,192-byte response takes
27.729 seconds to generate. Source telemetry's 211,834,613 bytes is not total
transport traffic. Successful ranges from the seed capsule alone total
5,429,552,383 requested bytes while their interval union is 349,365,585 bytes.
The ranges are distinct but heavily overlapping; exact-range duplicate counts
would miss this amplification. The reproducer
`dense_selected_pack_reads_each_source_window_once_across_batches` selects
100,002 objects from a 100,003-object pack in OID order. Before the fix, a later
batch requests another 2,888,431 bytes with only 140 bytes left in the source-size
budget. Proven selections now resolve once and sort by logical pack identity
and offset before the unchanged 50,000-entry batches. The sort runs off the async
executor; cancellation and the original aggregate byte budgets remain enforced.
Canonical and embedded-source cases each read exactly the source body once;
native Git strictly validates the exact selected OID set, including a forward
REF_DELTA whose base is emitted in a later batch. Existing thin, corruption,
cache and cancellation cases pass (32 pack tests and 24 real-Git integration
tests); strict all-target crate Clippy passes. Ordering by logical pack
does not prove globally optimal coalescing across multiple logical packs sharing
one source object. The 32 promised
blobs fetch in 3.889 seconds; the separate integrity checks take 16.693 and
15.439 seconds. No proxy errors were recorded. A full new-candidate replay,
controlled performance comparison and remaining parity gates stay open.

The installed physical-order candidate (`98f8ca5f21ce3ab5837f9f7758f1a075e0c8d23df334ddf831691bf381ce84bb`)
then ran against an independent copy of that frozen repository, compared with
the previous installed candidate (`806820af681765230187ad983fb1c5c6deef8808d4f49c5803047e3db43d17d9`).
Both prefixes started without generated responses or coordination objects. All
six objects accessed by the fetch were fully SHA256-checked against the original
and each other before timing. Each copy contains 5,195 v2 objects, 4,465,023,495
bytes; historical bodies outside the six-object read set were not fully rehashed.
Compilation and copying finished before timing; OS/backend caches were not reset.

| Filtered initial fetch | OID-order baseline | Physical-order candidate |
| --- | ---: | ---: |
| End-to-end command | 40.559 s | 36.921 s |
| Pack generation | 26.478 s | 8.384 s |
| Metered requests | 43,717 | 1,655 |
| Received bytes | 5,902,417,072 | 559,370,884 |
| Response objects / bytes | 1,113,655 / 213,284,192 | 1,113,655 / 213,284,192 |

Both runs passed strict Git checks before and after recovering 32 omitted blobs,
exact tip/content checks, released participating leases, and left no private
temporary files or repository publication. Sorted response-OID digests match
(`a3dc18547c97136e6d27b41aa6513ec30cc3ebb502457ba8e3521c398ef0c186`).
The baseline's 52 checks versus the candidate's 51 reflect four versus three
distinct participating lease keys, not relaxed assertions. Candidate seed-capsule
range overlap fell to zero; one layer still has 4,891,580 overlapping bytes.

This proves a 96.2% request and 90.5% received-byte reduction for this workload,
not a few-second clone or full release qualification. Generation improved 68.3%,
but whole-command latency improved only 9.0%. Candidate multipart cache-upload
requests reached 3.581 seconds versus 0.379 seconds in the baseline; the interval
from assembly completion to the helper's pack-ready event grew from 2.278 to
10.756 seconds. Those intervals include publication/verification work, not just
network latency. Foreground cache publication and client-side completion require
separate profiling; this single shared-host pair is not a controlled latency SLA.
Retained reports are `wire-oid-order-ga-20260927-r1` (SHA256
`e86837056175c5271c47886cce0b09b6e6cb775125c153fe707de23ab9714cab`) and
`wire-physical-order-ga-20260927-r1` (SHA256
`02267007e16ff08a5d0a52565fbd31ea38bda87ecb7285dd4a65c89a419c62f8`), under
the qualification volume's `Github/crabbuild/crab-capsule-cache-qualification`.

The earlier process-group signal case is not graceful-cancellation proof: Git
forwarded SIGINT again and the helper explicitly took its second-signal exit.
Reader-slot PUTs are required coordination, not repository publication. The
revised fault probe allows only those exact lease keys, forbids source/ref
writes, and requires released lease tombstones after cooperative cancellation.
The failed attempts and their logs remain retained; no deadline was relaxed.

`crates/crab-remote/tests/checkpoint/external_delta.rs` reproduced two native-Git
failures: kind queries over raw thin scratch packs could not resolve objects,
and complete installation exposed raw thin packs that Git rejected as corrupt.
Maintenance now repairs private source copies for kind queries while preserving
the exact selected object set and remote pack identities. Native installation
separately orders dependencies and repairs only dependent packs under their new
content identities, checking the exact source-plus-base OID set before install.
Self-contained sources do not take that repair path. Repeated installation also
reproduced a false destination conflict; existing bodies and sidecars are now
verified before reuse rather than treated as conflicts or trusted by filename.

All twenty-one checkpoint fixtures pass, including uncheckpointed delta chains,
repeated content-named installation, byte-identical native reads, the two-object
thin replacement, stable descriptor/ref preservation, corrupt-base rejection,
missing/cyclic-base rejection before any local pack is installed, and cancellation
without root publication. The corruption fixture damages the
base entry, not an unread pack header. These are component proofs using native
Git and in-memory storage, not current-binary RustFS qualification.
The 49 Git pack/related tests, 30 capsule-reader tests, five S3 capsule tests,
and nine HTTP maintenance tests also pass. Strict all-target Clippy for
`crab-git`, `crab-read`, and publication-enabled `crab-remote` passed; the
minimal-feature CLI check passed with 18 warnings. The repository frontend
was rebuilt before HTTP tests, with dependency/bundle warnings. No new release
binary or timed replay is qualified by these checks. Native repair may copy a
base into multiple local packs; that amplification still needs live measurement.
These functional checks do not by themselves prove private-runtime task drain;
that lifecycle proof remains open. Long-lived HTTP runtimes retain their
service-owned shutdown; private upload-pack runtimes require a separate owner
audit rather than inheriting proof from this checkpoint helper.

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

The September 27 pre-repack Kubernetes diagnostic reduced Git negotiation from
17 rounds to one and fetch latency from 10.656 to 4.460 seconds, while origin
requests remained 80. Seventy-two of those requests read 24 distinct capsule
objects. Reducing the three ranges per capsule to one cannot meet the ten-read
target; publication or independently scheduled physical maintenance must bound
the number of new physical sources before the fetch, without adding a
whole-repository rewrite or blocking ordinary pushes. This remains an open
design/performance gate, not an implemented guarantee.

A bounded ordinary-fetch read now retains complete, pointer-verified frontier
runs when their aggregate size is at most 128 MiB. The same authenticated bytes
supply run controls, pooled/original indexes and pack entries, while larger
frontiers and stable checkpoint sources remain lazy. On a fresh 500-push
Kubernetes frontier this reduced 24 capsule sources from three GETs each to one,
and total origin operations from 80 to 32. It does not meet the ten-operation
gate: physical source fan-out and eight setup/admission operations remain.
Full-run decoding and retained bytes are bounded costs, not free optimizations;
qualification must keep testing latency, RSS, integrity and large-frontier
behavior before v1 can be retired.

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

The harness now records parent-session child timings and retains selected
credential-redacted upload-pack diagnostics, including command failures.
Observed helper and installer times can overlap; they are not additive CPU
measurements. Missing or unfinished traces are marked incomplete. The small
[unpack-policy comparison](../benchmarks/capsule-v2-kubernetes-5000-rustfs-ga.md#fetch-phase-attribution-no-evidence-for-changing-gits-unpack-policy)
did not demonstrate a keep-pack speedup, so normal Git settings remain unchanged.
These diagnostics do not replace the complete Kubernetes performance gates.

Repack timing is split into inventory selection, selected-source download,
external-base reads, disjointness proof, structural concatenation or fallback
recompression, sidecar construction, candidate validation, upload, and CAS.
Every phase reports CPU, wall time, bytes, and attempts.

The CLI's existing `bytes_read` and `bytes_written` fields retain their shipped
v1.2.4 pack-body meaning. They must not be populated from total before/after
inventory or a source-count heuristic. The physical maintenance owner must
report selected-body work and new output-body work, including work completed
before a lost publication CAS; metadata, sidecars, external-base reads and
transport retries require separate accounting. Dry-run and genuinely
metadata-only/no-op maintenance report zero pack-body I/O. In r6's first
interval, the CLI claimed 1,157,197,029 body bytes read while the independent
proxy recorded only 269,799,447 total response bytes. That was a reporting
defect, not evidence that the stable pack was reread.

`crab/src/cmd/repack/capsule_tests.rs` adds CLI-level regression
fixtures: a nine-source dry-run must preserve the root and report zero body
I/O, while a three-source roll-up must report only its two small source bodies
and replacement body, followed by a zero-I/O no-op. The latter derives output
bytes from the published layered descriptor, not another CLI estimate. These
tests reproduced the reporting defect: dry-run reported 66,031 bytes read and
written instead of zero, and the three-source roll-up reported zero input bytes
instead of the selected 108. The layered implementation now returns
`CheckpointOutcome` from the maintenance owner: deduplicated installed source
bodies, replacement bodies submitted to verified immutable publication, and
root-publication status. The CLI combines logical and physical work instead
of estimating it from source counts. A forced logical roll-up is included;
metadata-only, dry-run and no-op passes report zero body work. Losing the root
CAS does not erase completed work.

These retain the v1 logical pack-body-work scope, not transport accounting.
An identical immutable output still counts the replacement body submitted by
that attempt, even if the provider returns an already-present result. Such
verification, duplicate wire transfer, request retries, sidecar/envelope bytes,
range overread and external-base reads must be measured independently. No
claim of newly allocated storage or exact network bytes follows from these
fields. The existing failing CLI assertions remain unchanged; the owner tests
also cover zero-work logical/no-op passes, source-limit work, lost-CAS work and
identical-output retry. Current-source verification passes: two unchanged CLI
regressions, six checkpoint-owner unit tests, twenty-one checkpoint integration
tests, five S3 capsule tests and nine HTTP maintenance tests. Strict all-target
Clippy for publication-enabled `crab-remote`, formatting and diff checks pass.
The minimal-feature CLI test build still emits feature-related and linker
warnings. The no-FUSE release build passed; the fresh RustFS replay remains
pending after the meter corrections recorded in section 13.2. These component
results do not qualify performance or retire v1.

The critical regression signal is `stable_pack_body_bytes_read > 0` during
ordinary checkpoint construction or a warm incremental fetch.

## 12. Implementation sequence

Each phase lands with one canonical path and focused proof. Later phases do not
ship while the earlier contract is bypassable.

### Phase 1: Freeze contracts

- Add bounded `PackSourceDescriptor`, `PackMemberDescriptor`, `PackLayer`,
  `PackLayerControl`, and `PackLayerPointer` codecs.
- Replace the development run formats with `CRBRUN06` aggregate member/locator control, exact
  OID-to-member admission, and a bounded control bundle so one physical run opens without nested control
  range fan-out. Inline small transaction/visibility/catalog sections, but
  keep larger sections as authenticated body ranges and fetch them only when
  materializing that control. The remaining trailer-discovery read is removed
  when the authenticated pointer carries the suffix range.
  Include the exact admission sidecar in that suffix; authenticate its hash and
  boundary before exposing the control view, without a second range request.
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

Current-main integration audit (September 27, base `de215cd0c49`): the newer
HTTP Cell browse projection still opens a v1 `RepositorySnapshot`, requires
persisted commit-graph/path-state descriptors, and uses their identity before
promoting a projection epoch. The capsule reader's synthetic manifest does not
populate these descriptors. Selecting the old capsule side of the maintenance
conflicts would therefore lose working main behavior, not complete browser
parity. This remains an implementation gate: bind derived browse indexes to
the captured v2 state (including per-ref changes), retain bounded resumable
index construction and corruption repair, and reject superseded projection
promotion. Keep import receive limits and deferred readiness, and retain the
current HTTP 202 indexing / 503 corrupt-metadata behavior without interactive
history scans. Existing main attribution assertions must remain behavioral
proof; deleting them is not a resolution. Capsule integrity scrubbing now uses
the current CellNode task owner so shutdown can drain its leases. All ten
textual merge conflicts are resolved. The UI build, workspace formatting check,
and HTTP library-test compilation pass on the integrated source; both focused
maintenance cancellation tests pass. These checks do not establish browser
parity: the native HTTP receive fixture reaches a successful tag push, then
fails while reading the absent v1 manifest for attribution. Its original
default-stack attempt aborted before that boundary; rerunning with the existing
CI `RUST_MIN_STACK=8388608` setting exposes the manifest failure. No assertion,
timeout, or production stack policy was relaxed. The read-path gap remains an
implementation and release gate, not a merge-conflict cleanup task.

The three approved behavioral assertion replacements also pass on this
integrated source: one exact byte-budget/install test and both S3 layered
checkpoint mutation tests. The S3 capsule filter passes five tests in total,
including receipt retry, read-view, and corrupt-root fail-closed checks. These
focused results do not cover the failing browser attribution path or replace
fresh release-binary scale qualification.

The next bounded regression reproduced a second reader gap: all three
`RemoteGitRepository` snapshot constructors discarded supplied graph/path-state
indexes. They now reuse the normal graph verifier and retain path-state
metadata, without looking up a v1 manifest or locator. A changed materialized
Git digest drops the base indexes even when the generation is unchanged.
Twenty-seven repository-opening tests pass, including exact attribution,
corrupt metadata, request/byte limits, cancellation, deadline and shutdown;
absent or superseded indexes cause zero index-origin requests. Two real-Git
snapshot integration tests also pass. This completes only the index-consumption
prerequisite. Strict library Clippy, workspace formatting, and the downstream
incremental-install byte-budget regression pass on the same source. Durable
capsule-state-bound index publication, resumable build
reuse, repair, HTTP source-token attachment and stale projection rejection
remain to be implemented and verified. Ordinary capsule manifests still omit
these indexes; no new push-side storage requests were introduced by this step.

A further prerequisite regression showed that `git_repository_from_store`
discarded its supplied origin before the first checkpoint. It now retains that
placement for both frontier and checkpoint readers. Both reader constructors
use one `git_snapshot` implementation: its synthetic identity covers visible
per-ref transactions as well as the root, and its pack inventory deduplicates
identical content while rejecting conflicting metadata. Full/control views
agree before and after checkpointing; a ref-only publication invalidates the
old snapshot even when the root digest and generation remain unchanged.

The first implementation exposed two regressions during targeted review:
missing uncheckpointed pack-byte admission and redundant origin reads of
already verified capsule bodies. Dedicated tests failed for both before their
fixes. Admission again rejects before I/O, and verified materialized bytes and
locators are reused while retaining the origin for other data. All 36 focused
checkpoint tests pass, including four new behavior cases. The existing
external-delta fixture was then extended and passed exact three-object-chain
reconstruction through this reader and native Git installation. The change
removes 29 net production lines without new configuration, dependencies or
persistent metadata writes. All 30 shared capsule-reader tests, five downstream
S3 capsule tests, and strict reader-library Clippy also pass. On this same source,
the native HTTP test completes its push, then fails again at the unchanged
`receive_tests.rs:342` v1-manifest attribution setup. No assertion or timeout was
weakened. This does not complete the background browse-index publisher, HTTP
projection parity or current-release RustFS qualification.

The next integration slice implements the opt-in `v2/browse-indexes` record.
Its 4 KiB ceiling and exact capsule-state binding keep it separate from ref
authority: native Git and mutation-validation readers do not load it. Background
maintenance shares v1's renewed generation owner and both GC writer fences,
reuses verified graph/path prefixes, drains graph reads in bounded batches,
and persists path-state progress every 32 commits. It rechecks a freshly loaded
root and visible ref positions before conditionally publishing both complete
index descriptors. HTTP attaches only a matching record. Cell projection builds
from the same captured snapshot, rather than rereading refs halfway through,
and retains its final complete-source-token promotion check.

Two further failures were reproduced, not hidden by changing assertions.
The first native HTTP rerun passed attribution but exposed a later valid branch
creation/deletion failure: visibility application accepted an authenticated
closure borrowed from another ref, while fetch-transition construction rejected
it. The shared full/control transition builder now accepts that creation case;
existing refs still require their own exact expected-old tip. Separately, graph
and path-state uploaders returned success for corrupt existing objects because
HEAD succeeded. Their shared uploader now verifies generated content identity,
checks existing bytes, and repairs only the observed version with CAS and
readback. This is derived-index repair, not permission to overwrite Git/Xet data.

Focused metadata, reader, generation and checkpoint suites pass 92 tests. The
native HTTP scenario passes for both `main` and `trunk`, retaining default-branch
and non-fast-forward rejection, Git clone/read checks, HTTP 202 while indexing,
and 503 followed by exact attribution recovery for missing/corrupt descriptors
and malformed/oversized records. The same expanded scenario passes against the
retained RustFS 1.0.0 GA instance. This uses the test profile and small Git fixture,
with local/in-memory Cell fixtures; it is not a release performance or durable
fleet qualification. Strict lint also exposed an integration-only nine-argument
receive adapter. Its forwarding layer is removed; server-owned options and
typed error mapping remain at the HTTP boundary. On the final source
`73747afb`, strict library Clippy passes for metadata, read, write, remote
(publication enabled), and HTTP. The default-feature CLI library check and
two payload-only metadata codec tests also pass. The final HTTP main/trunk
rerun passes in 23.65 seconds; a fresh-prefix RustFS rerun passes in 15.13
seconds. Its six small-fixture pushes take 137–209 ms, not a scale/performance
qualification. Workspace formatting, diff checks and source/lock hashes pass;
no request, timeout or integrity assertion was weakened.
The approved pre-payload byte-budget regression and all five S3 capsule cases
also pass again against the unchanged final Rust source, including the layered
checkpoint and exact ref/transaction-position preservation assertions.

This closes the reproduced absent-v1-manifest attribution dependency, not every
browser release gate. Large-history construction and restart reuse still need
qualification; graph discovery is not itself persisted before the complete
derived record, although path-state checkpoints are reusable. Derived-index
retention/GC, aggregate index-read allocation bounds and deterministic races
around stale publication/Cell promotion need further audit. Fresh integrated
release-binary Kubernetes/Xet qualification, matched v1 performance, the full
provider/product matrix and green PR CI remain open. No v1 retirement claim.

The following allocation audit reproduced post-download size enforcement in
both graph and path-state loaders: a descriptor one byte above the configured
ceiling was fully consumed before rejection. Both now use the shared storage
bounded verifier for descriptors and layers. Descriptor budgets are checked
before body consumption; authenticated layer sizes and the aggregate budget
are checked before each layer read. Hash verification remains mandatory, and
successful reads add no HEAD or extra GET. New behavior cases cover descriptor
overflow, aggregate exhaustion, oversized stored layers, and exact-limit reads;
storage probes cover truncated/oversized streams and wrong hashes as well.
The two regressions failed before the fix and pass afterward. All 15 graph/path
tests, 27 repository-reader tests, 38 checkpoint tests and the main/trunk HTTP
scenario pass; 16 focused storage checks and strict scoped library lint pass.
This bounds encoded intake, not the memory occupied by decoded indexes.
The current repo GC sweeps only capsule, checkpoint, pack-layer and history
prefixes; derived-index reclamation remains unimplemented, not implicitly
qualified by their exclusion from those candidates. Whole-root backup copies
include derived metadata, but live restore/projection proof remains required.

### Phase 7: Update fsck, history, recovery, and GC

The September 26 release-binary probe reproduced a CLI history gap twice:
listing retained history succeeded, but verification rejected its valid format-5
checkpoint through the embedded-checkpoint loader. The working-tree fix now
loads layered checkpoints, shares full-source validation with strict fsck, and
uses the native installer for authenticated thin-pack repair. It also shares the
current-view integrity path's reachable Crab/LFS pointer scan and origin-content
proof, rather than treating a catalog-only check as a complete dependency proof.
Restore checkpoints
the current view through the shared layered publisher, revalidates that result
before fencing, and reuses historical sources and visibility under a new ref
epoch. Its external catalog retains both historical and current dependencies.

These changes are newer than the binary used for the completed r9 Kubernetes replay.
Focused compilation and 80 tests pass: eight CLI history/recovery, five strict
historical-fsck, 65 metadata capsule-protocol, one restore-epoch race, and one
shared dependency-verification test. Strict library Clippy passes for metadata,
read, and write; formatting and diff checks pass. The minimal-feature CLI test
build retains 17 disabled-feature/linker warnings. Rebuilt-binary live history
verification/restore remains pending. Added regression fixtures cover historical refs/bytes,
thin-source dependencies, corruption without root mutation or leaked sweep
leases, and post-restore publication. Strict verification currently reads full
sources and then member ranges for native installation; no minimal-request
claim is made for this administrative path. Layered strict-fsck coverage and the
ordinary replay do not substitute for the separate recovery/GC/Xet matrix.
The first focused run passed seven tests and exposed a metadata-only restore
failure: history required a newly compacted capsule run even though the layered
checkpoint retained every pack source. The metadata contract now permits zero
new transactions while preserving checkpoint, ref, and chain validation. The
regression rerun also verifies the pre-restore state through its new
zero-transaction retained history entry.

The next deterministic probe exposed two Xet integrity defects: a valid file
was rejected because the shared verifier looked up raw-byte hex instead of the
catalog's canonical Xet MerkleHash encoding, while a forged whole-file hash
with valid catalog/shard/xorb envelopes was accepted. Correcting only the key
encoding made the valid control pass but left the forged file accepted. The
verifier now shares CLI catalog-selected recipe loading and origin-only
whole-file reconstruction. Six focused tests pass, including missing, shortened
and reordered recipes, false whole-file hashes, ignored shard hints, shared
content proof without skipping conflicting sizes, missing-origin error identity,
and cancellation during a shard read. Eight existing origin-recipe tests also
pass. These source changes still require the rebuilt-binary live Xet/history
run; they do not by themselves close the large-file recovery gate.

The scale harness now records each retained checkpoint, verifies its historical
dependency closure, restores the oldest version in its isolated repository,
checks a fresh clone's file hashes, and republishes/fetches the current version
under the new ref epoch. Its default 100 GiB workload remains distinct from the
planned four-file, 512 MiB/file, five-version diagnostic run.

The rebuilt minimal-feature release binary
`f0a3181d9631b6f6c98018a80b312ecca5b7404be9bc9ffc72ed56bea00ecb6e`
passed compilation (18 disabled-feature warnings); strict read all-targets
Clippy and 65 focused read/fsck/history tests passed. Its first isolated run,
`xet-history-20260926-r1`, failed the unchanged deduplication gate. The seed
contained no large files: `crab add models/` returned success with no candidates
because this build disables `gix-pathmatch` and uses exact glob matching.
Explicit file selectors in later versions did stage files. This is not valid
large-file seed or performance evidence. The harness now uses `models/**`,
supported by both matchers, and requires every model to be an indexed pointer
before each commit. The failed run is retained; `xet-history-20260926-r2` uses a
fresh bucket with the same binary. Full product-feature/pathspec parity remains
separate from this minimal-feature protocol run.

The corrected r2 diagnostic completed five pushes/checkpoints, byte-checked all
five historical file versions, and strictly verified all five retained
checkpoints. Its 10 GiB logical history retained 2,168,089,856 xorb bytes (0.202
ratio); cross-repository reuse reconstructed the consumer's exact bytes. Restore
preserved external keys and the exact historical Git tip, but fresh-clone
hydration failed before data transfer: the pointer-catalog reader followed a
retired ref head and rejected its checkpoint position against the restored root.
A direct-endpoint, fresh-cache probe reproduced the same error. This is a real
recovery failure, not a passed qualification or a transport-latency explanation.
The deterministic writer test now reproduces it using a post-checkpoint push
racing epoch rotation. The metadata fix filters retired heads before resolving
activation records, matching Git readers; current-epoch errors remain strict.
All 66 metadata capsule tests and 20 writer capsule tests pass, including
catalog preservation after a fresh new-epoch publication. Strict all-target
Clippy passes for metadata (including file-index-reader), write, and
coordination; formatting and diff checks pass. The minimal release rebuild
passed with binary SHA-256
`51a67c535e7fa05ce3e07fec2a83a188324b4f4ff8d14635a650d3c2f7a35392`.
Its direct-RustFS probe, `xet-restored-recovery-20260926-r1`, passed all 90
checks across 27 commands. Fresh-cache hydration of the preserved failed clone,
an independent restored clone, and the fetched republished version each passed
all 24 file SHA-256 digests and strict native Git fsck. New-epoch publication
and fetch preserved the exact latest tip; all original external xorb/shard keys
were retained. Final Crab fsck passed with zero errors and repair failures.
The original r2 failure report remains unchanged. This closes the reproduced
recovery defect on the four-file, 2 GiB current-content fixture; it is not a
fresh full-history run or metered performance proof. The default 100 GiB gate
and its separate storage-capacity requirement remain unchanged.

The fresh `xet-history-20260926-r3` run on the same `51a67c53` binary then
passed all 302 checks across 188 commands. It started from an empty isolated
RustFS bucket and completed five large-file pushes and layered checkpoints,
cold cross-repository chunk reuse, clone/hydrate/dehydrate, byte checks for
every historical version, strict verification of all five retained checkpoints,
oldest-version restore, fresh restored hydration, new-epoch republish/fetch,
and final native Git and Crab fsck. Its four 512 MiB models retained
2,168,089,856 xorb bytes across 10 GiB of logical history (0.202 ratio).
This completes that diagnostic fixture end to end, not the default 100 GiB,
GC/fault, provider/product, or paired-performance qualification gates.

A separate caller-level regression reproduced a protocol-selection defect
twice: with a valid v2 root and a missing current activation record, the shared
file lookup returned a recipe from remaining v1 metadata instead of preserving
the catalog loader's not-found error. Protocol selection now probes only the
root, then loads the catalog from that same verified snapshot. Missing v2
dependencies cannot select v1; genuine v1 repositories still work without a v2
root. The regression covers both acceleration modes and successful retry on
the same lazy handle after repair. All 28 file-lookup tests and strict metadata
all-target Clippy pass. The change adds no root request or storage-format change.
The minimal release rebuild passed with 18 disabled-feature warnings and
SHA-256 `0a01611df8772a24fcb2a1a04639fdb4eeb07490b87a1d2dd9f69e19471bb949`.
Its isolated `lookup-fault-20260926-r2` RustFS probe passed 40 checks across
28 commands. A real atomic branch/tag push published a 4 MiB Xet file; healthy
hintless-pointer hydration first proved the lookup path. Removing only its
backed-up activation record then made two fresh-cache hydration attempts fail
with the exact dependency path, no v1 manifest/layout/index requests, and an
unchanged pointer file. Restoring the record byte-for-byte allowed hydration
with the same second cache; file SHA-256, authority-object bytes, exact refs,
native Git fsck and Crab fsck passed. The activation was restored before exit;
its backup and both fault traces remain retained. The initial r1 harness attempt
used the wrong root key and stopped before fault injection; its failed report
is preserved. This live probe complements the in-memory v1-coexistence and
same-handle retry regression; it does not reproduce those two conditions or
substitute for the broader fault/concurrency matrix.

A subsequent GC audit reproduced a root-fence leak in both a deterministic
missing-run test and the live `gc-cleanup-20260926-r1` missing-checkpoint probe
on binary `a3637ae8`. GC acquired its root fence, then returned from a failed
view load before reaching release. The missing object error was correct, but
later publications remained fenced. The disposable fixture's checkpoint was
backed up and restored byte-for-byte; its original failed, fenced root is
retained as evidence. The working-tree fix includes view loading in the sweep's
cleanup boundary and preserves the original error. Its regression now passes,
including immediate GC retry without waiting for the separate sweep lease to
expire. History-pruning preparation was moved before fence acquisition;
restore already contains fallible post-fence work within its release boundary.
Eight history/recovery tests and three existing capsule-GC retention tests also
pass. The minimal-feature CLI Clippy run with `-D clippy::all` failed with 109
errors and 502 warnings; it is not a green strict-lint result, and full baseline
attribution remains pending. Its diagnostics do not point into the changed GC
or history-pruning cleanup functions. A separate minimal-feature library run
using the existing CI correctness/suspicious lint rules passed with 502 warnings.
That narrower result does not establish default-feature or full CI success;
neither lint rules nor thresholds were changed. Formatting and diff checks pass.
The subsequent minimal-feature release rebuild passed in 14m55s with SHA-256
`7a89365765617e4026a80920c5877d61474f441fbd669d2a61f07c4f2e96b748`.
Fresh RustFS probe `gc-cleanup-20260926-r2` passed 24 checks across 27 commands.
The exact missing-checkpoint error remained visible, the root fence cleared,
and byte-identical restoration allowed immediate GC retry without lease expiry
or repair. Logical root fields and exact refs were preserved; a subsequent
push, fresh clone byte comparison, native strict/full Git fsck and Crab fsck
passed. The binary was unchanged throughout. The original failed r1 evidence
is retained. This closes the reproduced read-failure leak, not process-death,
abandoned-future or the broader GC retention/concurrency qualification gaps.

The same release exposed a separate product-parity failure in
`metadb-layered-20260926-r1`: deep metadata diagnosis passed before checkpointing,
but deep diagnosis and rebuild both exited 9 after a real layered checkpoint,
reporting that layered sources require object-store-backed installation.
Shallow diagnosis and strict Crab fsck passed on that same repository; root
bytes and refs stayed unchanged. Both commands still called the embedded-pack
consolidator, while their existing tests covered only empty v2 repositories.
The small probe ran during r10's seed integrity phase, after its measured seed
clone and before incremental push measurements.

The working-tree change routes both commands through the canonical reachable
dependency verifier: layered-aware native installation, checked Git traversal,
and origin Xet/LFS content proof. Rebuild performs this proof before either
no-op success or checkpoint publication, removing its former unverified
publication branch and three whole-repository consolidation calls. Catalog-read
accounting is returned by the verifier rather than repeating catalog reads.
New tests cover a nonempty layered repository, missing immutable source, and a
reachable file absent from the catalog before publication. These three passed
after r10 terminated; the release binary remained unchanged during that run.

The follow-up audit found a remaining proof gap: layered member-range intake
authenticates Git bytes, but does not authenticate unused source-container
framing. A fourth regression preserves every member range, corrupts the source
header, proves native range installation still succeeds, and requires both deep
metadata commands to reject it without changing the root. A fifth test presents
a complete source one byte over budget and requires a typed limit failure before
any object-store request. Both tests failed on the previous verifier: it accepted
the oversized source and did not reject the damaged framing. The fault fixture
uses the underlying in-memory backend to bypass the storage owner's correctly
enforced immutable-create protection.

The shared administrative verifier now reuses `verify_layered_source`, with
deduplicated aggregate source admission before reads. HTTP adoption and
background integrity consume the same proof; ordinary push/fetch does not gain
full-body reads. Source-body verification and native pack intake are separate
bounded phases, so strict verification rereads member bytes during installation.
Cancellation drains native installation before releasing its temporary database.
Tokio 1.53.1 cannot abort a started blocking worker when its join future is
dropped; the HTTP scrub caller already cancels and drains its proof future.
All five new metadata regressions, seven existing capsule metadata tests, eight
history/recovery tests, five strict-source fsck tests, and the shared-reader
large-blob dependency test pass. After rebuilding the HTTP frontend, all six
HTTP adoption tests and three background-integrity tests also pass, including
missing dependencies and lease-loss cancellation: 35 focused tests in total.
The approved incremental-install byte-budget regression and all five S3 capsule
tests also pass against the current source, including both layered checkpoint
and ref-preservation replacements.
Reader all-target Clippy passes with warnings denied. Formatting and diff checks
pass. A rebuilt release and a fresh passing RustFS metadata probe remain pending;
Docker Desktop is unavailable, and the active Colima VM does not mount the host
qualification volume. These local tests do not replace current-binary live
qualification or prove arbitrary-future abandonment safety.

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

The September 26 caller audit found an embedded-format reader/writer and two
publication owners. The CLI command uses `run_repack_from_root`, but push and
Xet tests invoked the older `run_repack` embedded writer. That entry point also
replaced the manifest-v1 API present in release `v1.2.4`; manifest tests bypassed
it through a private helper. Switching the existing real-Git manifest test to
the public entry point reproduced `NotFound .../v2/root`.

The working-tree cutover removes the CLI embedded writer and its duplicate
history-selection helper. `run_repack` again uses its released v1 manifest
implementation, with both manifest fixtures exercising the public API. Capsule
push and Xet tests now use the actual CLI's root-pinned layered entry point and
object-store-backed installation. The Xet case additionally checks a fresh
post-checkpoint Git database, exact pointer bytes, preserved refs/catalogs, and
a repeat repack with no body I/O or root change. All 15 repack tests and the
complete Xet dispatch test pass. The migrated simple-push fixture reaches the
layered CLI and measures 26 requests for logical checkpoint plus physical
maintenance, failing its unchanged 12-request repack ceiling; its six-request
incremental-push assertion passes. This exposes the current CLI cost rather
than a new cost introduced by restoring the v1 entry point. The request
assertion now runs after the round trip and GC checks so it cannot mask later
correctness failures. The complete rerun passes checkpoint/ref preservation,
post-checkpoint push, fresh native installation, and forced-GC grace/root-fence
checks before failing only that final 26-versus-12 request assertion. The
performance ceiling has not been relaxed, and this slice is not a green branch
gate. Whether the old complete-repack ceiling remains a requirement for the
two-phase design is an explicit pending decision, not an assumed test rewrite.

The following fixture slice moves compact, retained-history fsck, GC, history-prune,
reader control and HTTP lagging-checkpoint cases onto layered checkpoints.
The GC case now deletes four aged orphan kinds while preserving the live
checkpoint, history, retained run and external pack layer. It first failed
because four prefix scans were reported as three; correcting the two counters
made it and the forced-GC grace test pass. These are logical prefix-scan counts,
not provider pagination/retry request totals. The changed compact/fsck cases,
GC fence-cleanup case, eight history tests and five retained-source fsck tests
also pass: 18 focused CLI tests in this slice.

The control-view fixture now uses ordinary fetch's layered-footer entry point,
retaining its exact four-operation assertion and adding exact read-byte checks.
That exposed a real storage-reader mismatch: the encoder, pointer validator and
control decoder accept a footer-only checkpoint at offset zero, but the storage
loader rejected it. The loader now accepts zero offset while retaining its
nonempty-range and authenticated descriptor checks. The fixture covers both
zero-offset and body-bearing checkpoints. The pre-fix reader run passed 29 of
30 cases; its failing case reproduced the mismatch. Two post-fix builds lost
their output directories during compilation. The recovered build now passes
all 30 reader tests, including zero and nonzero footer offsets. The approved
incremental-install byte-budget regression also passes in the complete shared
checkpoint integration slice. The HTTP consumer rerun also passes all nine
maintenance cases, including the lagging checkpoint/concurrent suffix case;
performance ceilings remain intact.

The two remaining shared publication fixtures now checkpoint actual retained
capsule-run descriptors instead of constructing unrelated embedded packs. Their
stored-checkpoint checks preserve the source inventory, split-run transaction position, history
chain and unchanged three-write logical-publication limit. The second history
checkpoint retains the first checkpoint's sources before admitting the next
run. Their first stable writer rerun passed 19 of 20 tests: the split-run case
exposed that source descriptors rejected repeated pack identities which run
compaction legitimately preserves. A separate native-Git regression with 32
valid ref replacements and two recurring pack bodies reproduced the same
reader failure, so changing the placeholder fixture would have hidden a product
defect.

Source validation now accepts repeated physical members only when every content
commitment and Git descriptor agrees. One comparison is shared with remote
object reads and native installation; source offsets and member ordinals remain
unchanged. Conflicting lengths, hashes, sidecars, checksums, counts and external
bases still fail closed. The new real-pack regression verifies retained sources,
compacted ref positions, strict source decoding, direct object reads, native Git
installation and full strict fsck. It failed before the source fix and now
passes with all 22 shared checkpoint tests. The subsequent owner-suite rerun
passes 70 metadata tests (one existing synthetic benchmark remains ignored),
36 reader tests and all 20 writer tests, including the unchanged split-run
fixture and the new conflicting-evidence cases. All five S3 capsule cases and
all nine HTTP maintenance cases also pass after rebuilding the required frontend
(which emits third-party and bundle-size warnings). These 162 focused passing
tests do not replace live service, full CI or current-binary Kubernetes replay
qualification. All-target Clippy for metadata, read and remote (publication
enabled) passes with warnings denied; formatting and diff checks pass. The
request ceilings are unchanged.

The cutover audit exposed lossy history admission: reconstructing checkpoint
pointers discarded their format, and reconstructing run pointers normalized
their stored capsule count in both history and per-ref heads. Both now share
the root's validators and check the original fields, including prepared heads.
Four regression tests failed before their fixes; canonical, hash-consistent
malformed history now fails after one history-object read, before following
its predecessor. The subsequent capsule-suite run passes 74 metadata, 36 reader
and 20 writer tests, followed by all 22 native-Git checkpoint integration tests.
The payload-only metadata build passes 72 tests; the existing synthetic
benchmark stays ignored. Five S3 capsule cases and nine HTTP maintenance cases
also pass on this source, bringing the focused total to 166 distinct passes.
Scoped all-target Clippy passes with warnings denied; format and diff checks pass.
This does not retire the embedded format or relax any request/performance limit.

The subsequent owner-wide cutover removes the embedded codec, publication
wrappers, whole-repository consolidation branch and reader/installation branches.
Checkpoint pointers now require explicit format 5; missing, format 3 and format 4
pointers fail admission instead of selecting a compatibility reader. Three
format-admission cases reproduced before the change and now pass. The owner
rerun passes 72 metadata, 36 reader and 20 writer tests, with one existing
synthetic benchmark ignored. All 22 shared checkpoint integration cases also
pass, including byte-budget enforcement and native Git reconstruction. Four
retired codec-shape tests were removed and two format-rejection tests added;
fewer tests here does not mean a weaker gate.

CLI fetch, fsck, GC, recovery and background-maintenance consumers now use only
layered checkpoints. Ordinary fetch still uses footer-only admission; full
catalog/visibility consumers retain their complete metadata read. GC no longer
silently skips the sources of an unsupported checkpoint. The separate v1
manifest protocol remains unchanged. The local and remote `v1.2.4` tag targets
have different commit IDs but identical source trees, neither containing the
capsule metadata module. This is removal of an unshipped development format,
not retirement of v1. The default-feature CLI rebuild passes 56 focused
repack, fsck, GC, recovery, metadata and classic-fetch cases. Its separate
incremental-push round trip again reaches the final request assertion with
six push requests and all preceding reconstruction/GC checks passing, then
fails at 26 repack requests versus the unchanged ceiling of 12. The observer
records ten GETs, six range reads, six PUTs and four LISTs. These are fixture
backend operations, not a new RustFS latency result. The S3 rerun passes all
five capsule cases, including the stronger source/ref/transaction-position
assertions; all nine HTTP maintenance cases pass too. This totals 220 focused
Rust passes plus 43 replay-harness checks, alongside the separately failing
repack-budget test. Scoped all-target Clippy passes with warnings denied, and
format/diff checks pass. Format cleanup has not resolved the request-budget
failure or supplied a current-binary live qualification.
The capsule-run pointer loader still has a trailer-discovery branch for omitted
control offsets; removing that separate development-pointer compatibility path
and migrating its synthetic fixtures remains a cutover audit item.

The next maintenance change retains the exact successful root-CAS receipt and
complete checkpoint between logical and physical publication. Both phases remain
independent CAS operations; newer heads are not folded into the physical pass.
The reader binds retained metadata through the same full pointer comparison as
stored reads, rejects footer-only input, and enforces the checkpoint byte ceiling.
CLI repack and the metadata owner now use that shared pinned-view pass, while
HTTP/S3 background owners inherit it through their existing maintenance entry
point. Repack statistics describe this pass's last published inventory rather
than issuing a later root/ref scan that can include another writer's work.
This adds a shared publication-receipt owner and validated in-memory reader
boundary, while removing duplicate caller-side reopen/repack sequences; the
extra production code preserves phase accounting and publication provenance.

A regression reproduced five metadata GETs where the initial root and two ref
heads require three. It now passes at three, together with all 27 shared
checkpoint integration cases. The new cases cover a losing root CAS, later
independent heads, cancellation after logical publication, retained-checkpoint
admission and native Git reconstruction. Immutable readback policy and the
12-request CLI repack ceiling are unchanged. This is focused fixture proof,
not a new RustFS latency result or completed Kubernetes qualification.
The rebuilt CLI fixture confirms 19 repack requests, down from 26: five GETs,
six range reads, six PUTs and two LISTs. Seven redundant reads are gone, but
the unchanged 12-request gate still fails. The six-request incremental push
assertion and all preceding reconstruction/GC checks pass in that same test.
The default-feature test link again warns that the macOS `__eh_frame` section
exceeds 16 MiB; it builds and executes, and the observed failure is the explicit
request-budget assertion, not a linker or correctness failure.
There is still a contention-path inefficiency: if logical publication loses its
root CAS, this pass can attempt physical work against the prior checkpoint's
already stale root. The conflict test proves refs remain intact and accounts
for the discarded work; avoiding that futile physical attempt remains an
optimization follow-up before qualification.
The owner rerun passes 73 metadata, 36 reader and 20 writer tests, including
full/control pointer-field rejection; the existing synthetic CPU benchmark
remains ignored. All 56 selected CLI consumer cases, five S3 capsule cases and
nine HTTP maintenance cases also pass. Together with the 27 shared integration
cases, this is 226 focused passing Rust tests and one separate, known failing
request-budget test. Scoped all-target Clippy, formatting and diff checks pass.
No current-binary 5,000-push replay or full-CI qualification is claimed.

The following contention-path fix stops physical maintenance after this pass
has already lost logical publication's root CAS. Its regression first observed
92 pack bytes read and 60 written against the known-stale root; the fixed pass
does neither. Independent physical debt still runs when logical publication is
below threshold, without folding newer heads. All 28 shared checkpoint cases
pass, including winning-ref preservation and native Git reconstruction.

Run pointers now require explicit control offsets, sizes and footer hashes;
the unshipped trailer-discovery path is removed. Full and footer-only readers
validate the original descriptor before I/O and bind the same control fields.
Three new regressions reproduced missing-field admission, pre-I/O validation
failure and unequal full/control footer binding before the fixes. The rebuilt
owner suites pass 76 metadata, 36 reader and 20 writer cases, with the existing
synthetic benchmark ignored. Together with the shared cases, this is 160
focused passes on this source. The separate v1 manifest is unchanged. This
does not resolve the previously measured 19-versus-12 repack request gate;
current CLI/service, lint and live replay proof remain outstanding.

### Phase 9: Qualify and decide retirement

The pre-recovery local environment check found the September 26 qualification
directory (including r9/r10 reports) and candidate release directory absent at
their recorded paths. The Kubernetes input checkout and RustFS data directory
remain present, but Docker Desktop is unavailable and the RustFS endpoint
refuses connections. Historical measurements in this document are not a
substitute for those missing raw artifacts or a current-binary replay. Resume
qualification only with stable build/evidence storage, a fresh candidate binary
and a new run namespace; do not reconstruct a passing report from these notes.

After approval to use Colima, an isolated `crab2721` profile was created with
its VM files on the workspace volume: four vCPUs, 6 GiB VM memory and a
256 GiB container disk. The existing Colima profile, Docker context and its
running workloads were left unchanged. The new loopback-only RustFS container
uses the repository-pinned `1.0.0-beta.8-glibc` image digest
`040304b66e029a5cde4bed140b41513e925909839a9b912a40a98340610d1f66`,
with four CPUs and 4 GiB memory. Readiness, authenticated bucket creation and
byte-identical upload/download passed. All 18 replay-harness and 25 request-meter
tests passed. The fresh reader build passed after the prior artifact loss;
no new candidate replay result is claimed. A separate VM still shares host
CPU, memory and storage contention, so paired v1/v2 runs must retain that caveat.

The normal isolated `make install` completed for the run-pointer cutover source.
The fresh `capsule-v2-2721-20260927-r11` replay started at 01:58:40 UTC on
September 27 with binary SHA-256
`1f4847f2f7d35c2df2974e8c0fcf24da7172b44881b1aa30cb277b20bbd67c0f`.
Source, manifest and harness hashes were checked before launch, and the
Kubernetes input remains a clean read-only checkout. The workload includes seed
publication, 5,000 pushes, fetch-before-repack every 500, and final cold/warm
clones and integrity checks. It finished at 03:48:52 UTC with a performance-gate
failure after completing every correctness check. No task-owned compilation
ran alongside its timed operations, and the binary hash remained unchanged.
The retained v1 binary has an empty feature set, unlike this normal-install
candidate; matching-profile, matching-feature v1 qualification remains pending.

R11's first 2,000 pushes complete with six median requests and exactly 7.012
mean requests in each 500-push window. Latency is not flat: window mean/p95
is 270.75/583 ms, then 875.27/2,391 ms, then 1,354.42/3,466 ms,
then 1,169.43/2,780 ms.
Seed publication takes 326.616 seconds
and nine requests; the seed checkpoint leaves its one pack unchanged with
zero pack-body reads/writes, but still transfers checkpoint metadata. Seed
clone takes 46.028 seconds; strict native Git and remote Crab fsck pass.
Fetch-before-repack at 500, 1,000, 1,500 and 2,000 preserves the expected tips
and adds exactly one local pack, taking 27.634/23.438/50.057/51.186 seconds
and 104/125/104/125 requests. All four exceed the unchanged latency and request
ceilings. Raw logs show no seed-capsule or pack-layer requests in these fetches,
and no seed-capsule requests during interval repacks. All four repacks retain
the 1,099,723,385-byte seed pack and consolidate only the smaller suffix;
the 2,000-commit repack takes 101.915 seconds and 88 requests. Stable reuse is
working in these samples, but does not establish qualified latency.

The next two completed intervals illustrate the timing caveat without erasing
the earlier failures. Push-window mean/p95 is 1,196.53/3,507 ms for
2,001--2,500 and 241.96/511 ms for 2,501--3,000; mean request counts remain
7.014 and 7.012. Their fetches take 6.186 and 6.579 seconds with 126 and 106
requests, respectively, preserving exact tips and adding one pack each. Both
avoid seed-capsule and pack-layer reads, but still fail the ten-request gate.
The two repacks take 19.442/19.366 seconds, 88 requests each, retaining the
same seed body while rewriting the growing suffix. This is neither a flat
latency result nor a controlled speedup comparison.
At 3,500, fetch latency rises again to 38.729 seconds / 106 requests; tip,
connectivity and one-new-pack checks still pass without seed/layer reads.
That push window averages 593.80 ms / 7.012 requests with 1,555 ms p95;
its suffix-only repack takes 86.049 seconds / 88 requests. Source and harness
hashes remain unchanged through this seventh completed interval.
At 4,000, the next fetch passes tip/connectivity and one-new-pack checks in
42.302 seconds / 106 requests. Its push window averages 1,036.05 ms / 7.012
requests, with 2,239 ms p95. The eighth repack takes 58.004 seconds / 87
requests and leaves three packs: it retains the complete previous
1,200,442,882-byte pack set, reads only 42,713,846 new suffix-body bytes,
and writes an 18,769,912-byte layer. This is the expected geometric tier
transition, not a whole-repository rewrite; it does not close the fetch gates.

All 5,000 individual pushes and ten fetch-before-repack intervals now complete.
The final two push windows average 554.83/227.93 ms, with 1,800/529 ms p95;
both retain 7.012 mean requests. Fetches at 4,500/5,000 take 6.857/6.650 seconds
and 107/104 requests, preserving exact tips, connectivity and one new pack.
All ten fetches read 24 capsule sources and no seed capsule or pack layers.
The last two repacks take 20.620/12.620 seconds and 89/87 requests. No interval
repack reads the seed capsule; the final inventory contains three packs.

Across all pushes, the mean is 752.0958 ms and 7.0122 requests. The distribution
is 4,849 six-request pushes, one seven-request push, and 150 pushes using
39--42 requests. The sub-second overall mean does not establish flat latency:
several 500-commit windows exceed one second. Cold/warm final clone commands
complete in 48.532/57.390 seconds, each using 15 requests and downloading
1,313,624,311 bytes. Shared cache naming did not reduce measured origin bytes.
These measurements are not a few-second clone result or a matched v1 comparison.
Both clones pass strict full native Git fsck, match the exact source tip, and
match all 32 sampled source blob digests. Final remote Crab fsck passes in
160.063 seconds with 297 requests. Aggregate push gates pass (752.10 ms mean,
7.0122 mean requests), but p95 push latency is 2,408 ms and the windows are not
flat. Fetch p95 is 51.186 seconds and 126 requests versus unchanged ceilings
of ten seconds and ten requests. The harness exits with a performance failure,
not a correctness failure; this run does not qualify v2 or retire v1.

These warm fetches use the terminal Git wire path, not classic-helper direct
installation. At 500, Git Trace2 records a 16.343-second helper stage, an
overlapping 6.192-second index-pack child and a subsequent 10.265-second
connectivity walk. The 104 storage requests sum to 1.311 seconds of recorded
duration; overlap means these are not additive CPU/critical-path accounting.
At 1,000, helper/index-pack/connectivity durations are 19.549/7.229/3.357 seconds;
storage durations sum to 0.974 seconds. At 1,500, the corresponding durations
are 41.808/19.385/6.488 seconds, with 0.818 seconds of summed storage durations.
Capsule reads still span 24 source objects, with thirteen exact repeated index
ranges after payload reads in the second fetch. The rejected whole-pack-union
probe cannot explain this sample: its 16,811 selected objects are below the
100,000-object probe threshold and fit one 50,000-object assembly batch.
Delta-base lookup and index eviction need a separate controlled probe.
At 2,000, eighteen exact capsule index ranges repeat after the payload wave;
the final 61-byte payload range is wholly contained in an earlier
2,277,813-byte read of the same immutable source. This demonstrates redundant
origin I/O, but does not attribute the entire fetch latency to it. The next
reader regression must cover an omitted delta base inside an already fetched
window with index eviction, byte limits and corruption checks, before changing
dependency-location or range-buffer reuse.

A bounded read-only inspection of that immutable run sharpens the regression
case. Its footer, pooled indexes and admission directory match their committed
hashes; the containing index also passes its Git checksum and pack-identity
checks. The 61-byte entry at member 29 offset 37,217 has the index's CRC32,
is a `REF_DELTA`, and is absent from every visibility addition in that run.
The complete physical admission directory nevertheless locates it exactly in
member 29. `extend_frontier_object_admission` retains only visibility additions;
`git_repository_from_layered_store` passes this narrower map to the reader as
its object-location hints in the r11 candidate. A missing dependency can
therefore fall through to the broad preferred-index scan despite an authenticated physical
location. An OFS-only fix would miss this captured case.

The controlled test exercises that orchestration seam with an
unselected, physically present REF-delta base and unrelated frontier sources,
under index-cache pressure. It checks byte-identical output, no unrelated
index probes, and corruption/budget rejection. Reuse the existing complete
run-member OID map as placement evidence, rather than broadening visibility:
the latter also participates in cold-clone closure proofs and is not an
interchangeable index. Physical presence must not authorize a hidden want or
prove that a client owns a thin-pack base. The inspection itself did not change
production code or quantify latency attribution. Four diagnostic range GETs
totaling 378,642 bytes bypassed the replay meter; no candidate or runtime setting changed.
Even removing all repeated index reads cannot meet the ten-request gate while
the interval still needs payloads from 24 different immutable source objects.
The focused orchestration regression is now written in
`crates/crab-remote/tests/checkpoint/frontier_admission.rs`. It constructs a
visible REF-delta with a physically present but non-visible base and an unrelated
member, disables index retention, and admits only the exact two-index/two-entry
byte budget. Separate cases check a tighter budget and corrupt base bytes.
Native Git independently validates the 87-byte pack fixture and reconstructs
both blobs exactly. Only this test and its module registration changed Rust
files during the live replay; removing those additions reconstructs the launch
source hash exactly. The r11 candidate and harness remained unchanged.

After r11 finished, the test fixture was indexed with native Git because the
locked gix index writer rejects REF deltas. The reader regression then failed
at the intended seam: 3,575 fetched bytes attempted against its 2,311-byte
budget. The layered reader now augments its location hints with the existing
validated physical run-member OID map. It does not modify visibility admission,
selected-object authorization, client thin-base proof, or index/CRC/OID checks.
The same regression passes with exact 2,311-byte reconstruction, rejects a
2,310-byte limit, and rejects corrupt base bytes. This is focused red/green
proof, not a new Kubernetes latency result. The post-fix rerun passes all 29
checkpoint integration, 36 reader capsule and five S3 capsule tests, including
the approved stronger byte-budget and source/ref/transaction assertions.
Scoped format and diff checks pass. The r11 binary predates this fix; remaining
CLI/HTTP/minimal-feature/lint/CI checks and installed-candidate qualification
remain required.

Current-source CLI consumer checks subsequently passed 39 tests: 15 repack,
six classic capsule fetch, two promisor fetch, one LFS publication, one staged
Xet dispatch, five retained-history fsck, eight history recovery, and one GC
cleanup case. The Xet fixture verifies exact reconstruction, changed-chunk
publication and existing-xorb reuse, clone before and after layered repack,
no-op repeated repack, unchanged pointer catalog, and a cold cross-repository
consumer. The fetch cases retain hidden-object rejection, promises, shallow
boundaries, deepening/unshallowing and read-admission release. These are native
Git fixtures over an in-memory store, not the pending 100 GiB RustFS run or
current-installed-binary performance qualification. The existing macOS linker
unwind-table warning remains; these results do not establish a clean lint gate.

The current-source CLI push/repack regression was rerun after that reader fix.
It still passes its six-request incremental push, exact ref/reconstruction,
post-checkpoint push and GC checks, then fails the unchanged repack ceiling:
19 requests versus 12. The sequence is three ref-capture operations, four
control/admission ranges, five logical-publication operations, two selected
payload ranges, and five physical-publication operations. Four immutable
objects each require PUT plus readback on the fixture's unqualified backend;
the two root replacements each require their own CAS. Therefore, even removing
all six source reads cannot satisfy twelve while retaining this exact
two-publication/capture/readback contract. This is a protocol/cost tradeoff,
not permission to skip readback or relabel the provider. A single atomic
maintenance publication would alter the intermediate-checkpoint-on-failure
guarantee; that choice is awaiting user direction, and no such change or
threshold relaxation has been made. It would also require read reuse to reach
the existing target.

The next independent optimization cuts over unshipped runs to `CRBRUN06`.
R11's first interval fetch has an exact 104-request decomposition: eight
root/discovery/admission/ref/checkpoint operations and four ranges on each of
24 runs. Each run separately loads its footer, exact member admission, indexes,
and selected entries. The 32-leaf batching rule explains those 24 sources:
20 leaves plus runs of 32, 64, 128 and 256 capsules. This is structural fan-out,
not evidence that the object store or decoder consumed all elapsed time.

The admission bytes already immediately precede the footer. `CRBRUN06` makes
the authenticated pointer cover both as one suffix; no extra copies, payload
downloads, or storage objects are introduced. The decoder verifies the exact
boundary and the footer-bound admission hash before returning any placement
hints. Detached large visibility/catalog sections keep their separate verified
reads; pack bodies and the index pool remain outside the suffix. Full-run and
control readers bind the same pointer. The detached-admission loader and public
attach/range APIs are removed rather than retained as a second path.

A new leaf/compacted-run storage regression first failed at two reads versus
one, then passed with identical admission/member data and rejection of shifted
suffix boundaries. Corrupt admission fails both decoders; retired `CRBRUN04`
and `CRBRUN05` magic is rejected. Current-source checks pass 77 metadata, 36
reader, 20 writer and 29 checkpoint tests; one pre-existing synthetic CPU
benchmark remains ignored. This is not a new installed-candidate replay.
GitHub's latest release is `v1.2.4` (published September 14), whose Git tree
contains no capsule-protocol implementation. Readers and writers must cut over together on fresh qualification
prefixes; the retained r11 objects and binary stay untouched. v1 is unchanged.

Reducing compaction batch size is a separate, unimplemented tradeoff. An
operation-count model of 500 uninterrupted existing-ref pushes, with the
observed six-request base and generic immutable readback, gives:

| Leaf batch | Sources at 500 | Mean modeled push requests | Capsules copied by compactions |
| ---: | ---: | ---: | ---: |
| 32 (current) | 24 | 7.012 | 1,024 |
| 16 | 9 | 7.106 | 1,280 |
| 8 | 9 | 7.230 | 1,528 |
| 4 | 6 | 7.488 | 1,780 |
| 2 | 6 | 7.988 | 2,030 |

The current-policy model matches r11's first-window request mean. Copied capsule
counts are not byte or CPU estimates: capsule sizes differ. Retries, creation,
checkpoint work, and lease renewals are excluded. Even six sources plus the
observed eight setup/lease operations cannot reach ten fetch requests. Batching
alone is therefore insufficient, and no policy or quantitative gate was changed.
The control-suffix change removes one required read per admitted run; a reduction
from 104 to 80 in that trace is a projection, not a live measurement.
The current-source CLI round trip does confirm the corresponding maintenance
reduction: 17 requests instead of 19, with six-request push, reconstruction,
checkpoint and GC checks passing before the unchanged twelve-request ceiling
fails. The two-phase publication/readback floor is still thirteen, even without
source I/O. This optimization does not resolve that separate contract decision.
The follow-up CLI/service checks pass 15 repack, one staged-Xet dispatch, eight
history recovery, six classic fetch and five S3 capsule tests: 197 distinct
focused passes with the earlier suites. The 17 run-codec cases also pass with
no default features; they are not counted twice. Strict metadata-library clippy
with only `storage` enabled passes. A fresh normal-feature `make install` started
at 04:36 UTC in an isolated candidate directory, with the external per-worktree
target and no global installation writes. It completed: FUSE, non-FUSE and
cache-server release builds took 5m43s, 4m54s and 0.65s, respectively. The
installed candidate SHA-256 is
`f4ea278722e20ecfb0fa23420f76baf5d99f8a9dd12a8877bd913a448521c898`;
the Rust source, global binary and retained r11 binary hashes are unchanged.
The existing non-FUSE unused mount-helper warning remains.

The default 100 GiB, three-version Xet qualification started at 04:48 UTC in
fresh run `xet-100g-colima-20260927-r1`, using that candidate and an isolated
RustFS bucket. Fresh-bucket, capacity, conditional-write/conflict and symlink
staging checks pass. The run terminated failed during the initial push at
05:12:59 UTC: add completed in 764.166 seconds, but push failed after 398.946
seconds with `CRAB-E0030` for a missing xorb. The meter records that xorb's PUT
returning 500, then 404 on retry; there is no successful upload for that key.
RustFS logs show 30-second local disk-operation timeouts, `/data` marked faulty,
aborted reads and an `erasure write quorum` error. The underlying disk stall
and the subsequent 404 cause remain unproven; free capacity, no restart and no
OOM do not establish a healthy storage backend. A read-only follow-up finds
the xorb absent and no published main ref, so the failed push did not expose
an incomplete tip. Later versions, clones and recovery were not reached.
All 18 replay harness and 25 request-meter tests pass again. The failed report,
objects and local staging are preserved; cleanup is disabled. No new latency,
Kubernetes replay or full-matrix qualification is claimed.

A separate direct-endpoint recovery diagnostic started at 05:30:56 UTC with
the same installed candidate, source, staging and cache. Before retry, all 330
prepared xorb files existed at their indexed sizes (21,489,849,425 bytes total);
this is size/presence evidence, not a replacement for push-time content checks.
Authenticated origin listing returned 141 xorbs totaling 9,200,051,441 bytes.
During retry, guest samples showed 74--75% I/O wait and high I/O pressure; the
shared host had about 12 GiB of swap in use and unrelated Rust builds. The
push returned success after 549.042 seconds; the remote tip matches the source
exactly and source `git fsck --full --strict` passes. Independent remote fsck
also passes with no errors or repairs (201.712 seconds). The cold pointer/Git
clone completes in 1.521 seconds with the exact tip; this excludes large-file
hydration. Turn cancellation interrupted hydration before byte comparison;
the execution handle and process PIDs were absent at 06:17:30 UTC. The partial
clone and cache are preserved: 32 model files are full-size, not yet proven
byte-identical. Hydration and comparison of every byte in all 50 model files
and 500 code files remain required. The push result is not a controlled
performance comparison or a passing end-to-end recovery proof until those
checks complete. Both diagnostic reports are separate from the original
failed qualification.

After the isolated GC build finished, a separate resumed integrity run started
from that preserved clone/cache with the original frozen candidate. Exact
source/remote/clone tips and source strict Git fsck pass again. Its workspace
temporary directory is explicit, and capacity covers the remaining 36 GiB of
model files plus 32 GiB of headroom. This is recovery admission, not a reduction
of the fresh scale run's 220 GiB gate. Remote fsck passes again with no errors
or repairs in 166.316 seconds; resumed hydration and every-byte comparison are
pending. No Crab build overlaps this resumed run.

The retained recovery runner is pinned to RustFS beta.8. The independently verified
[RustFS 1.0.0 release](https://github.com/rustfs/rustfs/releases/tag/1.0.0)
is running in a fresh, separate Colima container and volume, pinned by image
digest. Fresh-bucket conditional create/update/conflict checks pass, as do two
tiny seed/incremental/checkpoint publications with exact tips and clean remote
fsck. Eight orphan controls were created and GET-verified at 06:36:59--06:37:00
UTC for later grace/GC tests; their timestamps are unmodified. These are small
contract/preparation checks, not bulk or performance qualification. This is not
evidence that upgrading resolves the observed stall; the beta runner and its
volume have not been upgraded or deleted. Provider version changes require new
qualification and an identical backend for any paired v1 comparison.
All new qualification uses RustFS 1.0.0 GA at image digest
`sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
The older runner is recovery evidence only, not a substitute for the GA replay,
large-file matrix, or paired v1 proof.

At 07:06 UTC on September 27, qualification directories disappeared during
verification. The recovery hydration had completed, and sixteen 2 GiB model
comparisons passed, but comparison seventeen exited 2 after the source and
clone directories disappeared. This is an interrupted proof, not a content
mismatch or a completed 100 GiB verification. The GA overlap smoke had passed
24 checks, including 304 shared chunks and exact reconstruction, but its raw
report also disappeared. A second GA probe passed both add/push entry points,
cold-cache reconstruction before and after layered repack, and strict Git/Crab
integrity before failing cross-repository add-time proof admission: zero remote
proof chunks and one locally prepared xorb for 65 chunks. Its report and fixture
then disappeared too. No assertion was weakened. The task issued no cleanup;
the source of the removal is unconfirmed. Missing evidence must be recreated;
a run whose inputs or artifacts disappear is invalid. Neither probe qualifies
the full GA matrix.

The full GA scale gate was restarted at 13:52 UTC on September 27 as
`xet-100g-ga-20260927-r1`, using the installed physical-order candidate
`98f8ca5f21ce3ab5837f9f7758f1a075e0c8d23df334ddf831691bf381ce84bb` and a
fresh isolated bucket on the pinned RustFS 1.0.0 GA container. At 13:56 UTC,
the unchanged harness had generated all 50 two-GiB model files and 500 code
files (100 GiB logical, 20 GiB distinct bases), and initial add was running.
Seventeen admission/workload checks passed, including real conditional-write
conflicts, the 220 GiB host-capacity requirement and symlink staging safety.
At 14:14 UTC, initial add and all 50 indexed-pointer checks had completed;
the seed commit existed and its push remained active (67 checks passed).
Seed publication completed at 14:22 UTC in 848.881 seconds and 757 origin
operations, with no proxy errors. Its 330 canonical xorbs total 21,489,849,425
bytes; one external shard is 21,782,449 bytes. Root/run presence and identical
serial-versus-four-worker chunk coverage bring the run to 70 passing checks.
These initial storage totals reflect the ten distinct bases, not qualification
of later edits, cross-repository partial reuse or retained-history recovery.
At 14:26 UTC the first layered repack completed in 1.289 seconds and 11
requests, preserving refs and the exact external xorb/shard inventories and
making retained history available. The run terminated failed at 15:27 UTC:
version 1's full add reached the unchanged 3,600-second command timeout
(exit -124), after its deferred-add/Git staging path succeeded. Seventy-four
checks passed, but the three-version workload, deduplication, every-byte
historical reconstruction, restore/new-epoch republish and final fsck were not
completed. The retained report SHA-256 is
`abbf0ea9d1a7fc7389050f8b9e7fbc8fbf5f30172ac10465a9fbb4a7b65cac2d`.
Cleanup remains disabled. Shared-host build activity and about 11.3 GiB
of swap in use at launch prevent treating its timings as an isolated latency
comparison. The separate four-file proof and earlier interrupted large run
remain distinct evidence.

A two-file diagnostic on that unchanged binary reproduced disk amplification
when a 64 KiB edit falls outside the duplicate hint's sampled windows: a 64 MiB
file added 67,117,416 raw-segment bytes, versus zero with the edit inside a
sample. Both cases passed independent clone/hydration byte checks and native
Git/Crab integrity checks. The small case did not reproduce a latency slowdown;
it does not prove the full timeout's cause. The working-tree fix retains direct
Xorb preparation after a full-hash mismatch. Its ordered-recipe recovery test
passes after closing/reopening staging, with exact bytes and zero raw-segment
usage; all 62 add tests with `gix-pathmatch` enabled pass. The normal private
release install produced candidate
`db3c345f2789a7247c6338064118163964fbdf2a24c353b7a20c26e38fee8a8f`.
The unchanged diagnostic then passed all 30 checks in a fresh GA bucket;
both edit locations produced zero raw-segment bytes, with independent
clone/hydration byte equality and clean Git/Crab integrity checks. Its report
SHA-256 is `248fae5b7c8658c3c4435af806e4e510d67c66a4a091895658462bbb274851b2`.
This proves the bounded disk-amplification fix, not the full timeout cause or
a production latency improvement. A new scale run remains required. This add
policy also exists on current main, so the evidence does not establish a
v2-introduced regression.

A fresh isolated build and new GA whole-xorb reuse smoke subsequently completed
with their artifacts intact. The approved byte-budget and two layered S3
checkpoint assertions pass again from freshly built targets; 43 replay/meter
harness tests pass. After those builds ended, the current installed candidate
started GA run `capsule-v2-ga-2721-20260927-r1` at 07:35:54 UTC in a fresh bucket
and run directory. The clean, non-shallow Kubernetes input, binary and harness
hashes were rechecked; host/backend free capacity was 667/213 GiB. This run
includes seed, 5,000 pushes, fetch-before-repack every 500, cold/warm clones and
strict integrity. It completed at 08:20:26 UTC with all correctness checks
passing and unchanged binary/harness hashes, but failed both fetch performance
gates. The [complete GA report](../benchmarks/capsule-v2-kubernetes-5000-rustfs-ga.md)
records 258.10 ms mean push latency and 7.012 requests, 15.948-second / 90-request
fetch p95, and 28.847/29.181-second cold/warm clones. Both clones downloaded all
three physical pack ranges. This does not replace the missing 100 GiB evidence
or resolve the unconfirmed removal cause; the architecture is not qualified.

The first GA interval completes with 500 successful pushes, a 296.056 ms push
mean, 680 ms p95, and 7.012 mean origin requests. Seed strict Git and remote
Crab fsck pass. Fetch-before-repack preserves the exact tip and installs one
pack, but takes 15.948 seconds and 80 requests. The trace confirms 24 capsule
sources at three reads each plus eight setup/admission requests: the control
cutover removes exactly one read per source versus r11's first interval, while
leaving structural fan-out unresolved. No exact capsule ranges repeat in this
sample. Git Trace2 records a 14.815-second helper, overlapping 2.023-second
index-pack, and subsequent 0.578-second connectivity check; summed origin
durations are 1.202 seconds, not critical-path attribution. Repack takes 17.377
seconds and 63 requests, reads 57,428,006 suffix-body bytes and writes 20,308,594;
the stable 1,099,723,385-byte seed pack is retained. This is one interval, not
flat-latency proof, a controlled speedup claim, or full qualification.

A separate current-binary GC fixture uses only repository scope; the older
bucket-GC qualification harness is not run. Two initialized disposable
repositories contain four orphan source/control objects each, with backend
timestamps at 04:58 UTC. Before the one-hour grace expires, both normal and
forced GC return zero deletions. Independent GETs verify all eight fresh
candidates and five out-of-scope controls byte-for-byte. A nonempty prefix
without a root also rejects initialization without changing its objects.
Aged deletion, live-source/history preservation, concurrent/fault paths and
external dependency checks remain pending. No grace limit or timestamp was changed.

Fresh GA evidence for the retained 06:37 UTC fixtures completed at 14:13 UTC as
`gc-retained-ga-20260927-r2`, using the unchanged installed physical-order
candidate `98f8ca5f21ce3ab5837f9f7758f1a075e0c8d23df334ddf831691bf381ce84bb`.
Fresh clones passed exact-tip and strict native Git checks. Normal and forced
repository-only GC each removed precisely four known, naturally aged orphan
objects (232 bytes), preserved all other captured immutable objects and fresh
grace controls byte-for-byte, kept refs unchanged and passed remote fsck.
Independent active source-byte totals were 13,950 and 13,967 bytes; previews
and actual GC matched. Repeated GC deleted nothing, confirming fence release.
An initial probe-only missing argument interrupted verification after normal
deletion. Its failed report is preserved; the corrected probe verified the
same binary, original object digests and refs before resuming without replaying
that deletion. All 169 accumulated checks pass. The eight removed fixture
payloads are recoverable from the report, whose SHA-256 is
`17b15d6be443e47c82e567035852d075e52be5d5c84406d3205c9cfb32aff664`.
This proves these two small GA repositories, not the pending large-file,
history-only source, concurrent-GC or full provider qualification gates.

The installed candidate reproduces an accounting bug: after a real push with
matching local/remote tips, both LIST and HEAD report a 7,308-byte live capsule,
and strict Crab fsck passes, but repeated GC previews report zero active bytes.
An automated positive-active-bytes assertion exits failed. The v2 sweep leaves
the four source-byte classes at defaults; only the test-only v1 path assigned
them. The working-tree fix classifies the existing unique run/layer listing
against the current and retained mark sets, without new storage requests or
changes to deletion eligibility. These are physical source-object bytes,
including embedded indexes, not raw Git payload bytes or checkpoint/history
control-record bytes. The expanded regression covers active-over-history
precedence, history-only and coordinator-protected sources, normal/forced
fresh-object grace, and aged collectibles. Compilation and all three focused
GC tests pass: the two capsule sweep regressions and the cleanup/fence-release
case. Formatting passes; installed-candidate green proof remains pending.
The existing macOS linker unwind-table warning remains. The failed Xet run's
binary and its original source fingerprint are unchanged and predate this
accounting-only working-tree edit.

Further LIST-to-HEAD race tests found two v2 GC defects. First, the sweep
discarded `CandidateDelete::Retained` and reported all planned objects as
deleted. The unit regression preserved a replacement but reported two objects
and 12 bytes instead of one object and six bytes. An installed-CLI probe over
RustFS, with controlled HEAD identity changes, reproduced four objects and
284 bytes reported reclaimed despite zero deletion attempts and byte-identical
retained objects, both normally and with `--force`. The working-tree fix counts
only confirmed deletions; dry runs still report the eligible plan.

Second, v2's LIST filter retained reader grace under `--force`, but its HEAD
policy inherited the shared deleter's force bypass. A same-identity candidate
made fresh at HEAD was deleted in the forced unit case. This is a meaningful
provider contract: [S3 ETags reflect content, not metadata](https://docs.aws.amazon.com/AmazonS3/latest/API/API_Object.html).
An installed-CLI HEAD-freshness fault probe also attempted four batch deletes;
the probe rejected those requests, and direct GETs confirmed the original
fixture bytes survived. V2 now keeps grace at both checks. Shared v1/bucket
deletion policy is unchanged. The four race combinations pass, as does the
failed-view fence-release regression. These changes add no object-store
requests. All eight focused checks now pass, including neighboring sweep,
v1 force, recreated-object and confirmation cases. The normal-feature install
completed in a fresh isolated candidate directory; binary SHA-256 is
`62ee1929ede2154d3d54e36f7d7975b49d4aab1ac7eaf1716b8f470c876932f6`.
Both installed fault probes now pass 30 checks each, with zero attempted
deletions and zero claimed reclamation in both modes. The identity probe still
uses 48 requests, matching its red run. These remain fault-injected checks.

The unmodified RustFS repo-scope verifier separately passes 84 checks: exact
preview and actual accounting, four aged objects / 284 bytes removed in each
of two disposable repositories, fresh grace controls retained under normal and
forced GC, unchanged refs, byte-identical live/out-of-scope controls, strict
fsck and repeat zero-delete sweeps proving lock release. Only the eight seeded
orphans (568 bytes total) were deleted, and their payload remains saved. This
proves the repaired GC slice, not the outstanding full history/Xet/concurrency
and product/provider release matrix.

The existing external-thin unit test does not cover the selected-object path
used here. A future base-reuse change must test that path with a base reachable
from a common commit, plus rejection/materialization of an unproven base;
physical pack membership alone does not prove the client owns that base.
At 02:26:54 UTC,
the shared macOS host reports 8,032.56 MiB swap in use and unrelated build/test
activity. Neither source fan-out nor host contention is grounds to remove
integrity checks or claim qualified performance. The full replay and final
integrity checks are now complete; the failed performance gates and remaining
release matrix still require work.

Push audit events through 1,500 place most observed latency inside the push operation:
internal mean times are 249.23/763.55/1,194.75 ms across the same windows,
versus 21.52/111.72/159.67 ms outside that boundary. Eight Git subprocesses
per push contribute summed mean wall durations of 132.98/444.24/703.21 ms;
average packed-object counts are 37.90/33.70/34.75. Even `git config` rises
from 0.98 to 26.63 to 44.43 ms. These are elapsed times, not CPU attribution.
Ordinary six-request pushes with at most 100,000 uploaded bytes also slow
(237.96/839.97/1,256.22 ms means), so larger payloads and compaction spikes
alone do not explain the drift. Shared-host scheduling/I/O remains a plausible
contributor, not a proven excuse or a qualified flat-latency result. Preserve
raw traces and compare a matching-feature v1/v2 pair in a quiet environment
before making a protocol speedup claim.

On September 28, the frozen `f990449d` release binary passed a fresh-bucket
RustFS 1.0.0 GA Xet subscale run: fifteen 512 MiB files, three versions,
22.5 GiB of logical history, 3,729 passing checks, and zero proxy errors.
All versioned pushes, layered repacks, cold cross-repository chunk reuse,
fresh-clone hydration, byte-exact historical checkouts, retained-history
verification, restore, new-epoch republish, native Git checks, and remote Crab
fsck passed. Retained xorbs total 5,407,593,957 bytes, or 22.38% of logical
history. The seed push uploaded 5.387 GB in 210.680 seconds and used 229
requests; the two incremental large-file pushes uploaded 23.47/23.34 MB in
2.352/2.252 seconds and used 62 requests each. This is large-file Xet traffic,
not the simple-Git-commit request target. The complete run measured 122.4 GB
of origin response bytes across repeated cold hydration, history verification,
recovery, and fsck, but the original meter did not attribute bytes by phase.
Per-phase transport evidence has been added to the scale harness for the next
run. The retained report and transport SHA-256 values are
`132975dee61c24a3310bdcbf9e71247432590e6cc5a3a894c2073f9248ca653f`
and `7df0cb50a2c331b29ec019fdd168b03cffd55670d328ccf22c84fa763f35d0a6`.
The isolated bucket and generated data were cleaned after success; reports
and logs remain. This subscale pass does not close the default 100 GiB gate,
read-amplification investigation, paired v1 comparison, or CI matrix.

The metered repeat `xet-scale-7p5g-20260928-r4` passed the same 3,729 checks
with zero proxy errors and cleaned its isolated data. Its 122,468,505,110
response bytes include 60,047,476,531 from three explicit retained-history
verifications, 40,004,246,086 from eight hydrations, 10,848,610,358 from
oldest-root restore, and 5,486,496,121 from remote fsck. Those measured
read phases account for 95.0% of the total; other setup, clone, push, and
inventory work accounts for the remaining 6,081,676,014 bytes. A single
cold 7.5 GiB hydrate read 6,780,719,446 origin bytes in 218.074 seconds;
rehydration from the same cache read 75,868,687 bytes in 124.214 seconds.
Historical verification grows across generations (10.82/20.01/29.21 GB):
its dependency proof verifies catalog xorbs, then reconstructs each distinct
reachable file version, re-reading shared content without a cross-file origin
cache. The verifier intentionally reads origin rather than a shared content
cache so an unavailable or changed remote object cannot be hidden by earlier
proofs. Reducing this recovery cost needs an equally strong current-origin
readability contract; it is not an ordinary incremental-fetch measurement.
This separates deliberate repeated integrity work from one cold read, but
the Python request meter and Colima SSH tunnel make these wall times unsuitable
as unmetered production throughput. The run's report and transport SHA-256
values are `6ee6a476c623c4168c3f8b47d14ac25dccab01af60fd2a1e4914853625fb372c`
and `9a27fea03fab5806e0ff5faad63b7d4129fde648b1959a7830825f5ebe5d77de`.
The default 100 GiB, paired v1/v2, and CI gates remain open.

The September 28 fresh-bucket GA run `xet-40g-ga-20260928-r1` exercised
twenty 2 GiB files across three versions (120 GiB logical history). Its 96
checks passed through all three pushes, layered repacks, retained refs/history,
cross-repository chunk reuse and byte-identical consumer hydration. The two
incremental large-file pushes took 4.731/5.261 seconds and 72 origin requests
each; retained xorbs were 21,535,614,886 bytes, or 16.7% of logical history.
The full cold-clone hydrate was deliberately interrupted at the shared
volume's 20 GiB safety floor, so the report status is **failed** and this is
not full Xet or clone qualification. The report and phase-transport SHA-256
values are `a40afcf6e6baf678a1e6a414e11f08a75fd7b567146b7179cbd20d7b539ee07e`
and `acc0083c8e0535da6748b664a06512683914ac73b5a89c8ec6b74ce873e20e9d`.
The harness's former two-copy capacity estimate admitted this run with about
153 GiB free even though source/staging, co-located origin, clone output and
retained caches exceeded its headroom. The harness now releases each
task-owned cache only after that phase's integrity checks pass, preserving a
fresh cache for every historical proof. It also releases the consumer source
and clone after their respective remote-reuse and byte-identity proofs, and
dehydrates the published source worktree after copying the independent
consumer fixture. The source Git repository remains available for later
recovery and republish checks. Its preflight budgets one hydrated
logical checkout, distinct source/staging/origin/active-cache copies and a
transient-work copy, plus a 20 GiB safety reserve; it would require 160 GiB
for this shape and 220 GiB for the default 100 GiB shape. Each hydrate
rechecks current free space
before starting, so shared-volume changes after preflight fail early. These
changes prevent the observed unsafe start and bound cache accumulation; they
do not create capacity or close the release gate.

A 500 MiB, five-version RustFS 1.0.0 GA rehearsal
(`xet-500m-phasecache-20260928-r2`) passed all pushes, repacks, the
independent consumer clone's byte-identity check, and the first full-clone
hydrate. Its second hydrate did not start: 22,398,337,024 free bytes were
below the 22,523,412,480-byte safety requirement because the source remained
hydrated and the same-cache rehydration intentionally retained its first
500 MiB cache. The report is **failed**, not Xet qualification or evidence
of data corruption (report SHA-256
`b7b224d635a67a3d7d57600828a7900cfecd5abbf2facb49be3d2365eb3ac653`).
The subsequent fresh-bucket rehearsal
`xet-500m-source-dehydrate-20260928-r3` passed 158 checks across 174 commands
on the pinned `f16f5d71` binary: five versioned pushes and layered repacks,
source dehydration, cold cross-repository byte identity, cold and same-cache
rehydration, five exact historical hydrations and retained-history proofs,
oldest-root restore, current-tip republish, strict Git integrity and remote
Crab fsck. No request-meter error occurred. Its 2,621,440,000 logical-history
bytes retained 529,358,175 xorb bytes (20.19%); incremental large-file pushes
took 389–408 ms and 34 origin requests each. The second hydrate began with
33,171,410,944 free bytes against a 22,523,412,480-byte requirement. The
report and transport SHA-256 values are
`99bc1582ab80184490eaa97496e713fb8608e9cd5de96c3517d986cd0b443385`
and `3385e7bee0ae6172b0280513db1571f271fe03c9a6bc3bdec4c4d24455f4bdc9`.
The isolated bucket and disposable checkout/cache were cleaned after success;
reports and logs remain. This passes the lifecycle regression at 500 MiB,
not the 100 GiB, paired v1/v2, provider, or release gates.

The same pinned binary then completed three 1 GiB, five-version rehearsals.
The first (`xet-1g-current-20260928-r1`) passed 158 user-visible checks but
its meter recorded one proxy `TimeoutError` and one 5xx response. That is not
clean transport qualification, despite the old harness's `passed` status;
the exact request was not retained. The harness now requires zero proxy errors
before marking a run passed (35 focused Python tests pass). Two fresh traced
repeats (`xet-1g-traced-20260928-r2/r3`) each passed 159 checks and 174
commands, including all hydration, historical, restore and fsck paths, with
1,511 recorded requests, no proxy errors, and no 5xx responses. The retained
1,079,205,926 xorb bytes are 20.10% of the 5 GiB logical history;
incremental pushes in r3 took 369–452 ms and 34 requests each. R3's report
and transport SHA-256 values are
`a3352f40192392cd4d760b1c871176a29dba1e7ba7772acffddf2d84b038241f`
and `a4ee78720a26c276b3a93aff5a1beeff19b17ad0bdffcf8b58c4c235dbae0fc3`.
The isolated data was cleaned; reports and logs remain. The one timeout's
cause is unproven; these bounded runs do not close the 100 GiB gate.

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

The completed r6 replay in section 2.5.62 passed all 5,000 pushes, ten
incremental fetch/repack intervals, seed/final integrity checks and independent
cold/warm clones. Pushes averaged 290.56 ms and 7.012 origin operations. Every
incremental fetch added one local pack without reading the stable seed capsule
or standalone pack layers. It nevertheless failed the unchanged fetch gates:
249.9 average origin operations and 17.367 seconds p95. Cold/warm clones took
28.784/30.265 seconds; a direct-endpoint cold control took 26.018 seconds.
The shared host and absence of a paired v1 measurement prevent a parity claim.

Sections 2.5.63–2.5.64 describe subsequent bounded index matching and pooled
lookup indexes. Their r7 replay also passed the full correctness workload, with
264 ms mean push latency and 7.012 mean requests. Fetch requests dropped to
110.4 average, but 13.076-second p95 and 122-request p95 still fail the gates;
cold/warm clones took 32.534/34.305 seconds. Earlier bounded Git and isolated xorb/shard
smokes remain historical evidence, not substitutes for current-binary full
replay, product/provider coverage, or the gates below.

The paired-workload v1 baseline started at 13:26:17 UTC on September 26 as
`baseline-v1-1.2.4-20260926-r1`. It uses the clean `v1.2.4` release commit
`76977b2af1970aa0bf88dee50c5f12a2006c626c`, built separately with the same
minimal-feature release flags, and binary SHA-256
`84aa83b899a6ff05d73abb5c650542f4f76931d0523b463424fe3ea4d853db68`.
Its source commit range, 5,000 commits, 500-commit fetch/repack cadence, harness
and proxy hashes match r7; its remote prefix and client directories are fresh.
The build passed with 17 disabled-feature warnings. At its first interval,
500 pushes, the incremental fetch passed tip/connectivity and pack-preservation
checks, installed one new local pack, and took 713.289 seconds / 229,060
requests; repack then took 65.079 seconds / 15,437 requests. The fetch trace
records 219,087 catalog-compacted-object operations, 138,524,999,982 total
response bytes, and 9,064 HTTP 5xx responses. The meter cannot attribute those
failures to its own forwarding versus upstream errors, so this is not a clean
latency baseline or evidence of general v1 inferiority. A direct-endpoint
control remains necessary. Replay was stopped at 13:53:41 UTC after 1,000 pushes,
during the second incremental fetch; its terminal report is failed, not running
or qualified. The old proxy forced client and upstream connections closed on
every request. Host socket pressure and proxy-originated failures invalidate
the timing comparison; the historical trace cannot attribute each individual
5xx. The meter now reuses both connections and distinguishes pre-response proxy
errors from upstream status codes. Clean paired reruns with the corrected meter
remain required. The v1 storage protocol is distinct from Git's wire protocol
v2, which that release can also negotiate. No thresholds were changed for the
comparison.

The new release build completed on September 26 with binary SHA-256
`2b609f43903d390c2aff51f8ff1919d7d111a9fccd49939065469a1fba261b4b`.
`candidate-accounting-native-20260926-r8` was explicitly stopped during seed
publication at 15:17:40 UTC after a meter defect was reproduced; its report is
failed with zero completed pushes, not qualification evidence. The streaming
proxy counted an already-consumed chunk as unread when an upstream send failed,
so rejection handling could wait forever for extra client bytes. A 96 MiB real
socket test reproduced the timeout. A separate idle-close test reproduced a
meter-created 502 from a stale pooled upstream connection. The fixes account
for client consumption before sending and replace an already-readable idle
connection before forwarding, without silently retrying an HTTP operation.
All 23 meter and 18 Kubernetes-harness tests pass, including following a rejected
streamed PUT with a GET on the same client connection. The fresh full rerun,
`candidate-meter-drain-20260926-r9`, completed at 16:05:28 UTC on September 26.
All 5,000 pushes, ten fetch-before-repack intervals, seed/final Crab fsck,
independent cold/warm clones, strict native Git fsck, exact tips, and 32 sampled
blob comparisons passed. The report deliberately exits failed because the
unchanged fetch request-count gate still fails:

| Operation | Latency | Object-store requests |
| --- | --- | --- |
| Seed push | 217.173 s | 9 |
| Incremental push | mean 284.41 ms; p95 581 ms; p99 992 ms | mean 7.012; p95 6; p99 40; maximum 42 |
| 500-commit incremental fetch | mean 5.630 s; p95 9.830 s | mean 106.5; p95 111 |
| Final cold clone | 32.192 s | 18 |
| Final warm clone | 30.447 s | 18 |

Every 500-push window averaged exactly 7.012 requests; window mean latency
ranged from 253.04 to 356.88 ms without monotonic growth. Every fetch installed
one local pack. Git Trace2 recorded ten automatic-maintenance invocations but
no fetch-triggered repack. Fetch response bodies totalled 597,309,319 bytes.
The complete raw trace contains 37,465 requests with no proxy errors or HTTP
5xx responses. Final strict
Crab fsck took 173.014 seconds and 295 requests.

The r9 cold-clone Trace2 attributes 23.378 seconds to the remote helper and
6.847 seconds to checkout's `unpack_trees`; it records no `index-pack` child
for this direct-install path. The meter records 18.992 seconds for the large
seed-capsule response, plus 2.002 and 0.213 seconds for two pack-layer responses.
Those response durations include forwarding and consumer backpressure: they
do not isolate backend bandwidth from verification or local I/O. A direct
transfer control and phase profiling remain necessary before assigning the
remaining clone latency wholly to CPU, disk, or RustFS. Full checkout time
must also remain separate from a no-checkout pack-transfer comparison.

The read-only `r9-clone-transfer-20260926-r1` control fetched that exact
1,148,153,632-byte seed range to `/dev/null` three times directly and three
times through the current meter. All responses retained the same ETag, range
and length; the meter observed exactly three successful requests and no proxy
errors. Median wall time was 8.628 seconds direct and 8.984 seconds metered
(including AWS CLI startup). This does not explain the recorded 18.992-second
clone response as meter overhead, nor establish RustFS as instant: the control
excludes pack verification, disk installation and checkout. Host activity,
including a release build, and uncontrolled origin cache state prevent an
isolated-bandwidth or exact client-overhead claim.

The final incremental fetch issued 107 requests: 96 were four range GETs each
against 24 distinct capsule objects. The remaining eleven cover read admission,
replica discovery, root/checkpoint and ref capture. This establishes physical
source fan-out as a remaining request problem. A read-only comparison of all
24 retained run footers with the recorded ranges identifies every read:

| Read per source | Total bytes at commit 5,000 | Owner |
| --- | ---: | --- |
| Run control suffix | 4,158,355 | `load_capsule_run_control` |
| Exact object/member admission | 541,144 | `load_run_admission` |
| Canonical or pooled Git indexes | 1,076,568 | lazy frontier index lookup |
| Git object-entry window | 69,623,803 | selected-object range reads |

The sources contain exactly 500 members: twenty level-zero leaves plus four
runs at levels 5, 6, 7 and 8 (32, 64, 128 and 256 members). The batched
32-leaf compaction policy explains the twenty unmerged leaves at this fetch
boundary. All recorded index ranges match the canonical index span or the
authenticated pool; object windows span the member entry bytes between Git
pack headers and trailers. This is attribution of the existing run, not a new
performance result. Within-object coalescing alone cannot put a 24-object read
below ten requests. Reducing that fan-out also needs measured push latency and
write-amplification proof; changing the compaction threshold alone is not a
qualified remedy.

Read-only analysis `r9-compaction-analysis-20260926-r2` verified those footer
checksums and mapped their members to exactly commits 4,501–5,000. The full
24 source objects contain 75,495,625 bytes, compared with 75,399,870 bytes
across the 96 recorded capsule ranges. On this selection, one bounded verified
read per source would add only 95,755 bytes (0.127%) while reducing capsule
requests from 96 to 24. This is a candidate shared-read strategy, not an
implemented optimization: sparse selections can have a different byte cost,
and byte budgets, source authentication, visibility and reader admission must
remain enforced before exposing data.

Replaying the writer's compaction policy over the exact 500 original capsule
sizes quantifies its request/byte tradeoff:

| Leaf batch size | Final sources | Compactions | Mean push requests with required readback | Capsule upload amplification |
| --- | ---: | ---: | ---: | ---: |
| 2 | 6 | 250 | 7.988 | 5.206× |
| 4 | 6 | 125 | 7.488 | 4.788× |
| 8 | 9 | 62 | 7.230 | 4.240× |
| 16 | 9 | 31 | 7.106 | 3.767× |
| 32, current | 24 | 15 | 7.012 | 3.225× |

The current policy's request model matches every recorded push from 4,501 to
5,000: six baseline requests plus 476 compaction-source GETs and fifteen pairs
of compaction PUT/readback GETs, totalling 3,506. Each immutable upload's exact-key
readback is present in the trace. Custom endpoints require this storage-layer
integrity proof; it is not a redundant GET to remove. Alternative policies remain
transport-model estimates, not runtime measurements. Upload amplification
includes each original leaf and
its later copies but excludes run footers, index pools and admission sidecars;
the model also excludes retries, lost CAS attempts and intermediate merge CPU.
The current-policy result also matches the observed final member inventory. A
four-leaf batch predicts six final sources but about 48% more capsule upload
bytes than the current policy. Neither that policy change nor full-source
reads alone can meet the total fetch request gate while the metadata overhead
below remains. No production compaction policy changed from this model.

The working-tree compactor now encodes/authenticates the selected batch and
older carries in one pass instead of repeatedly encoding a binary merge tree.
The writer runs this CPU work on a blocking worker and retains the same source
selection, immutable readback and publication rules. A paired debug-build
diagnostic over the same 32 × 256 KiB synthetic leaves took 14.245 seconds for
five binary-tree compactions versus 3.181 seconds for five single-pass
compactions (4.48×); complete output equality passed. This measures local
compaction work on a shared host, not release push latency. Mixed-level,
ref-only, missing-admission and size-bound tests pass, as do the 20 writer
capsule tests, minimal metadata feature tests and strict all-target Clippy.
The r9 replay predates this change.

The minimal-feature release rebuild completed in 18m12s with SHA-256
`a3637ae803ea5459877d1d0bb67385cca0087324b1116bdfaeadd951973a3897`.
`single-pass-compaction-20260926-r1` passed 135 checks across 412 commands:
128 incremental updates to one branch exercised batch compaction and mixed-level
carries, followed by exact fresh-clone/pull content checks, native Git fsck and
strict Crab fsck without repair. Updates used 903 requests (7.055 average) with
no meter errors; mean latency was 140.91 ms and maximum 212 ms on this small
synthetic fixture. This proves the release publication/read path, not Kubernetes
scale, paired speedup, or the full fetch gate. The complete rebuilt-binary
5,000-commit replay remains required.

The eleven non-capsule requests are also explicit in the trace: one root GET,
one replica-discovery GET (404), five reader-admission operations, two ref LISTs,
one ref-head GET, and one checkpoint-control range GET. Even a single capsule
read would therefore miss the ten-request total with this unchanged overhead.
The recorded admission sequence is create (412), GET, GET, conditional PUT,
and release PUT. The working-tree fix consolidates nonblocking and ordinary
contended acquisition, retaining the inspected payload's CAS version instead
of reading the released tombstone again. Its request test failed at four
acquisition requests before the fix and now passes at three for a new context
or two for a known key, excluding release. All 121 coordination tests pass,
including backend-age protection, a successor winning the conditional-write
race, retry/release behavior, and concurrent reader capacity. The documented
reader limit is unchanged. Live request qualification of the rebuilt binary
is partial: r10's 2,000-commit fetch records four reader-admission operations
instead of five. This removes one request, not the capsule-source fan-out.

The interrupted `candidate-gc-cleanup-20260926-r10` used release SHA-256
`7a89365765617e4026a80920c5877d61474f441fbd669d2a61f07c4f2e96b748`.
At 2,100 pushes its mean was 390.50 ms, p95 1,061 ms, p99 2,034 ms, and
7.011 mean requests. Every complete 500-push window averaged 7.012 requests,
but their mean latencies were 352.75, 241.46, 321.75, and 528.32 ms: request
flatness is proven for those windows, latency flatness is not. Fetches at
500/1,000/1,500/2,000 took 4.619/7.968/18.024/30.445 seconds and
104/125/104/116 requests, each preserving installed packs and adding one pack.
The 2,000-fetch trace attributes 4.35 seconds to Git index-pack and 4.25 seconds
to its connectivity rev-list; all 116 metered requests sum to 1.72 seconds
of request duration. Overlap and uninstrumented helper/host delays prevent
assigning the remainder to CPU or storage from this trace alone. Workspace
free space fluctuated between 12 and 19 GiB during this interval. No own
compilation ran, and no source, evidence or other project's data was deleted.
This is incomplete shared-host evidence, not a passing performance gate or a
paired v1 comparison; the running binary excludes the newer metadata fix.

R10 terminated at 20:36:51 UTC after 4,356 successful incremental pushes when
the RustFS upstream refused connections during push 4,357. The meter returned
502 and the client exhausted its retries; the report remains failed. The
successful pushes averaged 431.33 ms and 7.0145 requests. Eight interval fetches
completed, each adding one pack; final cold/warm clones, final sampled bytes and
final integrity checks were not reached. A post-exit SHA-256 check confirmed
the release binary was unchanged. The active Docker context was then `colima`,
the Docker Desktop daemon was unavailable, and the original host data directory
remained present. Colima's `/Volumes/Workspace` resolves to its internal root
filesystem, not the host workspace volume. No daemon/context reconfiguration,
data deletion or automatic restart was performed. Recovery requires an approved
Docker setup; this is not a completed 5,000-commit qualification.

Read-only trace attribution through push 2,000 records eight Git subprocesses
per push: config, ancestry, pack generation, index-pack, two cat-file calls and
two rev-list walks. The 1,940 ordinary six-request pushes averaged 350.73 ms,
with 149.20 ms summed Git-process duration and 50.63 ms summed store-request
duration. Sixty compaction pushes averaged 695.32 ms and 39.73 requests.
The two rev-list scans serve different ownership boundaries: per-ref visibility
and LFS dependency/path-lock publication. Sharing their graph evidence is an
optimization candidate, not permission to replace either proof with the set of
uploaded pack objects. Git process timings include I/O and scheduling; summed
request durations include parallel calls and cannot be subtracted as a
critical-path CPU estimate. Raw attribution and script hashes are retained in
`r10-push-trace-cost-through-2000.json` beside the qualification diagnostics.

Other-repository test activity was observed on the host, so these runs are not
isolated-host latency evidence. No unrelated process was stopped. The recovery
changes in phase 7 postdate this binary, and a clean paired v1 baseline, the
full Xet/recovery/GC matrix, and provider/product qualification remain required.

Use a fresh GitHub Kubernetes clone as the read-only source and an isolated
RustFS 1.0.0 GA repository. Record the resolved image digest and run both v2 and
the paired v1 comparison against that same provider version:

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

The September 26 `concurrency-20260926-r2` diagnostic passed on release binary
`0a01611d` (before the single-pass compactor): 128 independent branch creations,
256 updates, 128 fresh protocol-v2 clones and 128 incremental pulls, with exact
content and native Git fsck. Eight divergent same-branch pushes produced one
winner and seven structured stale-info rejections. Measured branch creations
used nine requests each and existing-branch updates six, with no root PUTs in
the push phases. Final Crab fsck passed without repair. The prior r1 failed
because the request meter used `select()` on a descriptor above its limit;
a real-socket regression reproduced the synthetic 502 and now passes using
the platform's default selector. All 25 meter/harness tests pass. This is a
synthetic Git fixture, not Kubernetes or large-file throughput evidence.

`concurrency-faults-20260926-r2` passed 22 checks across 152 commands on that
same binary: pre/post-publication SIGKILL, publication rejection, response loss,
eight concurrent rebase integrations, fresh clone/content proof and strict
Crab fsck with zero errors and zero repairs. Its predecessor correctly failed
strict fsck on the expired namespace lease left by post-publication SIGKILL:
existing-ref recovery reclaims the ref holder but deliberately does not acquire
the independent namespace lease. The stronger fixture now also creates a
sibling in the same namespace through ordinary push, proves backend-expiry
reclamation (21.798 seconds against a 21-second lease), preserves both exact
refs and verifies a fresh sibling clone. It neither repairs first nor ignores
expired leases; the original failure remains retained. These bounded probes
do not close the full GC, Xet-fault, provider or product matrix.

The September 27 GA rerun uses current candidate `98f8ca5f` and unchanged
harness `bae33311`. `concurrency-ga-20260927-r1` passed 12 checks across 2,514
commands: 128 branch creations, 256 updates, 128 independent protocol-v2 clones,
and 128 incremental pulls with exact content and strict native Git fsck.
Eight divergent same-ref pushes yielded one winner and seven `stale info`
rejections (exit 3), not missing structured responses. Branch creation measured
exactly nine requests each; updates measured six. Neither phase wrote the root;
all metered phases had zero proxy errors. Final Crab fsck found zero errors and
performed zero repairs. Concurrency was bounded to eight writers and two readers.

`concurrency-faults-ga-20260927-r1` passed 22 checks across 152 commands on the
same candidate. Pre-publication SIGKILL kept the ref invisible and fenced until
lease expiry; post-publication SIGKILL kept the committed tip readable and
allowed immediate same-ref recovery. Ordinary sibling creation reclaimed the
abandoned namespace lease after expiry. Persistent publication rejection
withheld the ref and returned structured indeterminate status; a lost successful
response reconciled to the exact committed tip. All eight concurrent rebase
integrations completed, fresh readers saw the expected bytes, and final Crab
fsck passed without repair. Candidate and harness hashes were unchanged afterward.

The respective retained report SHA-256 values are
`7baef2ef755b6733ce395702ffc32ef2395f0ade69a9c259309398829f8fcaa0` and
`47cc5578e345683590f86b92b6a078abf03d5d78d10b19a37115d18058aca01e`.
Both use isolated buckets on RustFS 1.0.0 GA through Colima and overlap the
100 GiB Xet run. They establish bounded correctness and request counts, not
isolated latency, 128 simultaneous writers, or the remaining full failure matrix.

The September 28 current-head rerun found a new lost-response regression. In
`concurrency-head0ba-20260928-r1`, the injected ref-head create reached RustFS
and a fresh reader saw the exact ref, but push returned `stale info`: the
storage retry had turned the lost successful reply into `AlreadyExists`, and
the publication layer classified that conflict before exact readback. A
regression test reproduced this failed outcome. The writer now compares the
persisted candidate after every failed conditional write; identical bytes
confirm commit, a different readable head confirms contention, and failed
verification remains uncertain.

The corrected release binary (SHA-256
`f16f5d7172f4f488f66459119c7fd2a6f39acaff6cbd69305ead35aec6c67c31`)
passed `head-lostreply-fault-20260928-r1`: 20/20 checks and 85 commands on a
fresh GA RustFS bucket, including a reached response-loss fault, visible exact
ref, successful push, fresh clone and strict fsck. The separate
`concurrency-lostreply-fixed-20260928-r1` run passed 29/29 checks across
2,587 commands: 128 independent branch creations, 256 updates, independent
protocol-v2 pulls, eight same-ref contenders with one winner, and strict final
fsck without repair. Its retained report SHA-256 is
`a2c0af726ff66f14474c10966e115513624493148cf231fd300d4565b2a8bfca`.
These runs do not establish the default 100 GiB Xet gate, v1 parity, or green
hosted CI.

The September 29 interactive-repack follow-up removes the earlier
17-versus-12 request failure without changing the background maintenance
contract. Explicit CLI repack consolidates the selected pack suffix and
publishes its logical checkpoint with one root CAS. A pinned frontier of at
most 64 MiB retains authenticated capsule bodies for repack; installation
reuses those bytes only after checking each pack and sidecar against the
source descriptor, while larger frontiers use bounded control/range reads.
The metadata owner and HTTP server retain separate logical and physical
publication/cancellation boundaries. Current-source reader and checkpoint
suites pass 37 and 38 tests, and the real-Git two-push, repack, post-repack
push, fresh-install, and GC round trip meets the unchanged twelve-request
repack ceiling. This focused fixture does not establish the larger live
provider, Xet, or v1-performance gates.

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
