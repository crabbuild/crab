# Cell read replicas: local RustFS qualification

The uninterrupted `plan036-local-5` run passed 3, 5, 10, and 20 separate
Crab containers and the four fault phases below. It started on 2026-09-26 UTC
(2026-09-25 in the operator's timezone). Each Crab node had an inspected
1 CPU, 1 GiB, no-swap limit and a separate Cell volume. All containers shared
one Colima host and the fixture's network namespace. This proves local
functionality; it does not qualify independent hosts or production capacity.

## Source and environment

- Runtime source: `75b6da1a97d`; the final edit at that commit was test-only.
- Runner source: `f8b164094e3787da0980244424d19c0215ad3b69`, clean checkout.
- Server image: `sha256:921dc5c7ec9ba004df65430a03a8cb9f1a0957e62a253f4578c876ed4e66b2ce`.
- Provider: the RustFS image pinned in `render.py`, with 65,536 soft and hard
  descriptor limits verified at every stage.
- Acknowledgements: object durability mode throughout.
- Raw report: `$HOME/.codex/cell-issue-fleet/plan036-local-5/read-replica-report.json`.
- Report SHA-256: `2c9c49e3bbe4091661ad0d505de6615e0193b553b5de0ec32718ca214d2e82d6`.

The runtime image was reused with `--skip-build`: its source and the runner
source are recorded separately. Later commits changed the qualification
scripts and documentation, not the runtime in this image. The stopped project
and its remaining volumes are retained for inspection.

## Read distribution and latency

Each mode ran 200 issue-detail reads at concurrency eight. The owner was the
ingress node for this Cell; the explicit replica route used the other nodes.
Every measured request succeeded and returned the expected issue. Counts
below come from the serving-node response header, not gateway distribution.

| Nodes | Readers | Reads per reader | Owner requests/s | Replica requests/s | Replica p50 | Replica p99 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 2 | 100 | 539.20 | 178.40 | 38.77 ms | 102.09 ms |
| 5 | 4 | 50 | 1195.84 | 211.49 | 36.60 ms | 59.54 ms |
| 10 | 9 | 22–23 | 1373.04 | 150.67 | 50.17 ms | 78.01 ms |
| 20 | 19 | 10–11 | 1000.01 | 53.66 | 134.42 ms | 302.03 ms |

All selected readers were proven ready at each stage. Distribution was even
over the selected readers. The primary was excluded from the replica route.
The workload shows no throughput improvement from adding readers. Every
replica request performs fresh authority and signed membership checks against
RustFS; the full membership scan grows with fleet size. These short,
single-host samples are not steady-state benchmarks or scaling guarantees.
The feature remains opt-in with the fresh authority gate intact.

## Sampled resources

These are the largest per-node samples collected after each stage, not peaks.
The raw report includes process status, descriptors, disk usage, and runtime
metrics for every node.

| Nodes | Server RSS | Server descriptors | Local Cell disk | RustFS descriptors |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 49,176 KiB | 85 | 1,300 KiB | 53 |
| 5 | 51,264 KiB | 92 | 1,596 KiB | 133 |
| 10 | 54,832 KiB | 156 | 2,288 KiB | 194 |
| 20 | 58,160 KiB | 259 | 4,440 KiB | 702 |

An earlier exploratory run exhausted RustFS's inherited 1,024-descriptor
soft limit during churn and received S3 500 errors. The fixture now declares
its provider descriptor budget before startup. The final run needed no
manual provider adjustment or resumed phases.

## Fault results

| Fault | Observed result |
| --- | --- |
| Kill one of two readers | Replacement served in 17.443 s; writer and epoch unchanged. |
| Kill primary only | A previously verified warm reader took over; new comment acknowledged in 10.767 s. Published root unchanged at takeover. |
| Kill primary and both readers; delete their three Cell volumes | A survivor recovered the identical digest, txid, checksum, and sequence, then recruited two readers. Whole recovery phase: 32.650 s, including volume deletion, checks, and restarts. |
| Pause RustFS | Replica request returned HTTP 503 `replica_unavailable` in 5.659 s without data. After unpause, the same incarnation and sequence 188 were readable. |

The all-disk-loss root was
`f983ddaa05c7508f2f489f953d8d16dbb0119f73a44abeb5ee73a15f5e443e39`,
txid 131, checksum 12561710048701396894, sequence 127, before and after
takeover. The recovered issue and label matched their acknowledged values.
Each timing is one observation, not a recovery percentile or SLO.

## Fleet-to-object rollout

The separate, uninterrupted `plan036-rollout-4` run passed at runtime source
`997082d38ebcd5694b4448136bf2616fb949bdec`, runner
`1f77d6a478de486ad439dec280dc32e26e5ce680` (clean checkout), and image
`sha256:c8ae5fda425560b34939631f7fd2a101264224cb485cb1c7c01e5d20d2b436af`.
It ran from 04:13:32 to 04:15:10 UTC on 2026-09-26. This image adds the
shutdown-order correction described below; the scale measurements above
remain bound to their earlier image.

The three-node fleet issued five follower-backed proofs while the bounded
workload acknowledged 37 comments. Every server then drained with exit code
zero and no OOM kill before the runner changed any configuration. After
restart in object mode, all original issues, labels, and comments were
readable. A new comment completed with object proof; the new processes
reported seven object proofs and zero fleet proofs in aggregate.

The runner then killed all three servers, deleted their three project-labeled
Cell volumes, and restarted fresh node sessions. It verified all 38 comments,
all three issues and labels, and two replacement readers. Both readers served
ten checked issue reads. The provider and its volume survived throughout.

This test exposed an existing host ordering defect: cancelling heartbeat
maintenance before runtime drain attempted session withdrawal with an open
log and produced an error exit. The host now joins work producers, drains the
runtime and closes the covered log while heartbeats remain live, then stops
lease maintenance and withdraws. The same task ceiling and absolute deadline
bound both phases. The regression failed before the correction; all 35 host
tests, including both stalled-task phases and shared task-limit checks, pass.
Host/server Clippy and the release container build also pass.

Raw receipt: `$HOME/.codex/cell-issue-fleet/plan036-rollout-4/mode-rollout-report.json`.
SHA-256: `5a32a2bf9d06f34494477b203b20a64b5479a4ef953eb6699c092f14ea7dab31`.
The earlier failed drain is retained in the `plan036-rollout-3` state directory;
its fleet configuration and volumes were preserved. This establishes the
local offline rollout, not a rolling platform upgrade or a provider-outage
qualification during the drain itself.

## Framework client follow-up

Later changes moved the ingress load selector into runtime `ReplicaReadRouter`
and added explicit `ReadPolicy::Replica` to `CellClient` and
`ApplicationHandle`. HTTP issue-detail queries use the same runtime router.
The container measurements above precede these changes and describe the earlier
round-robin route. Focused selection tests cover equal-load rotation,
busy-reader avoidance across Cells, and cancellation/timeout cleanup.

The runtime exact-root test exercises local and authenticated peer routes,
default owner reads after a write, stale snapshot reads, minimum-position
rejection, target withdrawal without owner fallback, owner-ordered streams,
and fencing after release. Its RustFS run uses the isolated prefix
`plan036-client-policy-20260926`.
Generated application clients also read and refresh a real SQL snapshot, report
its actual receipt, and keep commands on the owner. The primitive scenario sets
replica policy while verifying that Queue, Effects, and Workflow activity lease
checks still use the owner before external work.

The application fixture originally used LTX defaults that disagreed with its
compiled storage limits. An existing generated-client test reproduced the
failure. Bootstrap and takeover now obtain limits from the same compiled Cell
type; production admission remains intact.

The product mTLS E2E uses in-memory storage and local RustFS (isolated prefix
`plan036-shared-router-20260926`), including explicit HTTP replica reads,
minimum-position rejection, 32 concurrent routed mTLS reads, target withdrawal,
and fenced takeover. That test has one reader; it validates route integration
and authority behavior, not multi-reader load distribution.

## Sparse snapshot follow-up

The change introducing this section replaces full reader restores with an
immutable SQLite VFS over the existing authenticated page resolver. A view
keeps an empty private placeholder; page bodies use the shared bounded 8 MiB
cache and SQLite's managed 64 KiB page cache. No capture session, WAL, or
checksum sidecar is constructed. Disabling `query_only` still cannot write.
The synchronous opener uses SQL-worker admission in the runtime, independently
of LTX's blocking-I/O admission, and retains resources through cancellation.
Sparse query faults inherit the query deadline and preserve provider errors.

Each runtime view now reserves a provisional 12 MiB and four descriptors.
The charge conservatively covers the complete shared page cache plus view
and fetch overhead. It is an admission reservation, not a measured RSS result.
Refresh charges both old and new views until old queries finish. Library and
product test fixtures were sized to admit both views; production capacity
rejection remains enabled. The prior 8 MiB product fixture correctly returned
pending readiness rather than bypassing the new 12 MiB admission charge.

Local evidence for this change:

- All 25 existing Cell-root tests passed, including sparse writable publication,
  hydration, compaction, and process-kill recovery.
- A new 2 MB payload test fetched fewer than 512 KB for opening and a small
  query, without materializing database pages. It also proved a one-slot LTX
  directory-cache pool can progress, deadline rejection, missing-object and
  checksum-error propagation, and byte-identical reads after provider repair.
- Exact-root A/B isolation, write rejection, placeholder cleanup, and cancelled
  installation tests passed.
- Runtime snapshot/refresh/fencing tests passed in memory and on local RustFS,
  using prefix `plan036-sparse-reader-20260926`. Reader placement and five
  generated-client tests passed.
- The real mTLS product test passed on local RustFS under prefix
  `plan036-sparse-http-ready-20260926`, including 32 concurrent routed replica
  queries, policy withdrawal, and takeover recovery.
- LTX/runtime/application/HTTP all-target Clippy passed with warnings denied;
  formatting, crate layout, and 66 documented Rust snippet checks passed.

The earlier container images and throughput measurements predate this sparse
implementation. The fresh-image run below qualifies its multi-container path.

## Sparse reader container qualification — 2026-09-26

An uninterrupted local RustFS 3→5→10→20-node run passed, starting at
`2026-09-26T05:33:18.971219+00:00`. Runtime and runner were the clean source
`d6904ba187de801f087a611900c7e71e3e9a2d0b`; image:
`sha256:555c381cf1092a60dc982e18d9008d689c569b458d99722493cfd638bd94fbd3`.
Every application node retained the inspected 1 CPU / 1 GiB / no-swap limits.

Each workload contains 200 requests at concurrency eight. The owner route
uses the ingress's local owner; replica routes involve remote readers. These
short samples are not sustained-load or throughput-scaling evidence.

| Nodes | Owner requests/s | Replica requests/s | Replica p50 / p99 ms | Actual requests per reader |
| --- | ---: | ---: | ---: | --- |
| 3 | 3,152.57 | 530.08 | 13.70 / 66.29 | 98, 102 |
| 5 | 3,051.56 | 411.72 | 18.63 / 33.29 | 50 each across 4 |
| 10 | 2,423.16 | 247.93 | 31.09 / 51.31 | 22–23 across 9 |
| 20 | 2,763.04 | 98.09 | 75.33 / 141.21 | 10–11 across 19 |

The instrumented authority now counts routing loads. Whole-node windows
observed 2.0 control loads per completed replica read at 3/5/10 nodes and
2.685 at 20 nodes. LTX bytes in those read windows were 0, 0, 1,440, and 0;
the 10-node window also included background page work. These counters exclude
membership/policy reads and provider-internal retries. Collection skew and
background work prevent interpreting them as exact per-request or total
billable S3 costs. Raw labeled series and collection timestamps are retained.

| Nodes | Latest poll-observed freshness after ACK, seconds | Reader-node LTX bytes during refresh | Sampled process RSS, MiB | Open FDs | Local Cell disk, KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| 3 | 3.473144 | 67,686 | 44.4–49.1 | 46–88 | 756–904 |
| 5 | 2.802640 | 132,612 | 43.2–51.0 | 35–104 | 32–1,836 |
| 10 | 1.046711 | 284,895 | 45.2–54.6 | 44–149 | 32–3,128 |
| 20 | 2.189718 | 638,495 | 51.2–61.6 | 59–123 | 32–3,516 |

Freshness bounds include authority inspection and polling. Refresh bytes
include background work and query page faults. Resource ranges cover whole
application nodes, including owner work; they are samples, not peaks or
per-view allocations. The provisional 12 MiB view reservation is unchanged.

Fault results in this uninterrupted run:

- Reader replacement: 13.957 seconds; writer and epoch unchanged.
- Warm-reader promotion plus a new acknowledged comment: 8.158 seconds.
- Owner and both reader volumes deleted: identical root recovered and two
  replacement readers recruited; phase including node restart took 39.443 seconds.
- RustFS pause: HTTP 503 `replica_unavailable` after 5.151 seconds, then
  recovered with the same incarnation and observed sequence 161.

Raw receipt: `$HOME/.codex/cell-issue-fleet/plan036-sparse-cost-1/read-replica-report.json`.
SHA-256: `fd86dce3ab43ede8a00e56c99ff1189938dc7ccdb410be6e62637062c0b3ad3b`.
An unchanged copy is retained as `initial-read-replica-report.json`.

Two additional phases ran on that exact image, with separately committed
runners and separate receipts. They are not part of the uninterrupted run:

- Runner `41dcfea0dea09edf8172419d2d3bc78dc071c14b` repeated all-three-disk
  loss, recovered the identical sequence-149 root, recruited readers, then
  acknowledged and served a new issue-body update. Its epoch-4 writer
  advanced the durable root to sequence 152. Recovery, write, recruitment,
  and node restart took 22.342 seconds. Receipt `all-loss-write-followup.json`,
  SHA-256 `c2a01691debd1d95548788035c80589a2f08f732bbb9714d493d81cdc1e58249`.
- Runner `53cf478f316` exercised desired counts 0→1→2→4→1. Actual ready
  counts matched each target; zero returned `replica_unavailable`. A selected
  reader was killed before the shrink to one. Writer and epoch remained
  unchanged, with no root rewind. Observed convergence took 0.566, 0.796,
  1.101, 1.845, and 0.735 seconds. Receipt `reader-targets-followup.json`,
  SHA-256 `938151f3538b912063433ff336f6f0b598eda9ef30b587168de3d1bb164f1d0d`.

A third follow-up used runner `f5459d76d03af8367b5883b7fcd4eca18c135d9d`
with the same runtime/image. From `2026-09-26T05:59:19.203089+00:00` to
`2026-09-26T06:00:50.275902+00:00`, a paused per-node S3 proxy isolated
`node-02` from RustFS while preserving HTTP and peer networking. Before the
partition it demonstrably served replica queries. During the partition it
returned typed HTTP 503 after 5.005 seconds while `/livez` still returned 200.
After killing the primary on `node-10`, healthy `node-15` acquired epoch 4,
recovered the identical sequence-289 root, and acknowledged a new comment at
sequence 290. Recovery and the write took 9.658 seconds.

The isolated session never became owner. Its authoritative session expired
and its public listener closed, producing an empty gateway 502. The node
process was still draining, with no OOM kill. This agrees with the lease-watch
cancellation path in `src/server.rs`; the qualifier requires a fresh expired
session before accepting that response. Connectivity and the original
endpoint were restored, both affected nodes returned healthy, and the proxy
was stopped. Receipt `reader-partition-report.json`, SHA-256
`764597c215322a84b317ceaf581bd87b698ce583877198577ea870cc2ce3decd`.
The earlier setup failures and the stricter error-shape assertion failure
remain in separate logs; they are not passing receipts. Seven Python evidence
checks pass, including rejection of an empty gateway error without expiry.

## Reader lifecycle follow-up — 2026-09-26

Source `04bf77cced9ed058574c494bfa08db5971917d74` fixes two failures reproduced
by a paused real SQL query. Node drain used to return before replica blocking
SQL completed. Also, cancelling the query and dropping the reader used to
release its 12 MiB/four-descriptor reservation while the SQL still held the
view. The SQL pool now closes admission and drains the existing job ledger;
the blocking task retains the complete snapshot reservation until it exits.
Final job admission and pool closure use the same lifecycle lock. This adds
25 net production lines and reuses the existing admission system.

Both regressions pass against in-memory storage and local RustFS. The tests
verify charges while blocked, no early drain completion, rejection of the
closed reader's result, and release of resources and placeholders afterward.
A concurrent schema-migration test also passes: the owner and epoch stay the
same, code/schema and durable root advance, the old in-flight query and refresh
are fenced, and a fresh reader sees the migrated value.

Local RustFS prefix: `plan036-reader-lifecycle-20260926`. Native test log:
`$HOME/.codex/cell-issue-fleet/plan036-reader-lifecycle-1/runtime-rustfs.log`.
SHA-256: `dba75fc3ef6e1c1381fe836bcb49e377ccb0d867ffc09a2843449b549b518aab`.
The adjacent `receipt.json` records source, test, prefix, coverage, and scope.
Five worker, two shutdown, four migration, seven publication, and ten host
lifecycle tests pass. Runtime/application/host/HTTP all-target Clippy passes
with warnings denied, and the native HTTP server build passes.
This is native runtime/provider evidence; the earlier
Compose image and performance figures are unchanged.

## Reader drain and offline retention — 2026-09-26

The aged disposable RustFS fixture exposed two maintenance bugs that also
existed on the inspected `origin/main`. A bounded pass retired its operation's
session, then the next pass tried to recreate that permanent tombstone.
Also, the executor closed `CellRuntime` directly and final `CellNode` cleanup
attempted a second runtime shutdown, returning `RuntimeClosed` after Ready.

Source `db70890a75b` preserves tombstones and deterministically selects the
next unused maintenance session. Competing executors strict-create the same
identity, so only one advertises; a post-advertisement release check fences
delayed retries. Ordinary node creation and random-session backup operations
are unchanged. Source `287a5eb1397` routes terminal shutdown through the owning
`CellNode`. Tests now use the real host and its storage contracts, and check
both competing retry admission and final idempotent cleanup.

The successful uninterrupted run used:

- Runtime: `287a5eb1397c768f00f753bfc592c58236738a59`.
- Runner and test fixtures: `a62084b1b731e21940575c88272a96c8ac4ee049`.
- Image: `sha256:faf59e3a8b06bea7b4b57473428642a7677fd69b454ce3580150558d0fea381f`.
- Interval: `2026-09-26T07:08:33.669661+00:00` through
  `2026-09-26T07:09:51.491082+00:00`.
- Receipt: `plan036-sparse-cost-1/reader-retention-report.json` in the local
  qualification state directory; SHA-256
  `acd3ab8278b0a88a0634dfa75bfe831f22630ad83bc92a0feffab076a9ad84f1`.

Three constrained nodes served twenty existing issues, including two proven
readers for work-01. The first retention pass deleted one object and left
revision 8 in Maintenance. All three servers exited zero without OOM, and
their old sessions were no longer live. Retrying the same prepared revision
deleted another 5,455 objects, retained 823 reachable objects and both backup
pins, preserved 327 objects within grace, and returned revision 9 Ready with
exit code zero. Provider inventories matched all 5,456 deletions. No deleted
object was younger than one hour at command completion; timestamps and the
production grace minimum were unchanged.

The retained pin verified. After restart, all twenty issue titles, bodies,
and captured comments matched their pre-maintenance values. The observed Cell
kept its incarnation and advanced from sequence 319 to 325, with two readers
returning sequence 325; a new comment was acknowledged and read back. Values
also matched the snapshot from before the earlier failed attempt's sweep.

The earlier failed receipts and logs are preserved separately. In particular,
`reader-retention-partial-report.json` records a sweep that deleted 27,363
objects across two passes but failed final cleanup; it is not a passing run.
A separate disk-admission failure was resolved by removing only this task's
unused compiler-image layers, restoring 34 GiB free in Docker. No capacity
threshold was lowered. All fixture containers are stopped; volumes remain.

The singleton retry regression, three maintenance tests, ten runtime session
tests, nine Python receipt tests, all-target runtime/HTTP Clippy, formatting,
layout, and documentation validation pass. The final server image built
successfully. This closes the local offline-retention case; it is not a
concurrent online-retention protocol or a production release qualification.

## Reader loss before writer loss

`qualify_reader_first_loss.py` passed on runtime `287a5eb1397`, runner
`9bfe045988f`, and the same `faf59e3a8b06` image as retention. Both readers
(nodes 1 and 3) were killed and their local volumes deleted first. Replica
reads became explicitly unavailable. The surviving writer on node 2 remained
at epoch 11 and acknowledged a new issue body, advancing the exact S3 root
from sequence 331 to 332 in 0.886 seconds after reader loss.

The writer was then killed and its volume deleted. Three fresh nodes recovered
the identical sequence-332 root at epoch 12, returned the acknowledged body,
and recruited two readers. Recovery plus node and reader checks took 14.037
seconds. This proves write acknowledgement is independent of surviving read
secondaries in object mode.

Receipt: `plan036-sparse-cost-1/reader-first-loss-report.json`, SHA-256
`ad4272a7e3a7aa6965906655a1256131680c89b05270fee2a5d36794665ef02e`.
Interval: `2026-09-26T07:15:06.214552+00:00` through
`2026-09-26T07:15:57.106930+00:00`.

## Sustained reads across unequal ingresses

`qualify_reader_load.py` passed on runtime `287a5eb1397`, runner `3819b4565b2`,
and image `faf59e3a8b06`. Five nodes retained the 1 vCPU/1 GiB limits. Six
closed-loop clients used node 1 and two used node 5, for sixty seconds on each
route. Every response matched the previously acknowledged title/body; both
modes had zero errors. Every replica receipt was at least sequence 362.

| Route | Successful reads | Requests/s | p50 ms | p99 ms |
| --- | ---: | ---: | ---: | ---: |
| Owner | 16,752 | 279.10 | 24.35 | 85.23 |
| Replica | 4,345 | 72.39 | 98.55 | 326.63 |

Node 1 sent 3,256 replica reads, distributed 811–818 per reader. Node 5 sent
1,089, distributed 272–273 per reader. Across both ingresses, the four readers
served 1,084–1,090 each. Selection balances work observed by each ingress;
this run does not establish a global load estimator. The longer workload,
forwarding paths, and changing fixture differ from the earlier 200-request
measurements; these numbers should not be treated as a controlled regression
comparison with those bursts.

The owner window recorded 17,722 control reads and 142 LTX origin requests
(247,866 bytes); the replica window recorded 9,621 control reads and no LTX
origin fetches. These are whole-node counters including background work,
excluding membership/policy requests and provider retries. Boundary samples
across both modes showed 65.3–89.0 MiB RSS, 117–337 descriptors, and
4,008–6,688 KiB local Cell disk. Process-lifetime RSS high-water marks were
also 65.3–89.0 MiB; these are neither per-reader peaks nor capacity proof.

The first attempt used twelve clients at one ingress and exceeded its existing
eight-request collaboration admission limit. It returned 89,881 HTTP 429s;
that failed receipt is retained as `reader-load-overload-report.json`, SHA-256
`e6d4677fc14028e46376173ff30b11115a9e9d7e3d58eda96bcc339e3a5733a4`.
Server limits were unchanged for the admitted run.

Passing receipt: `plan036-sparse-cost-1/reader-load-report.json`, SHA-256
`804d6de1ae30f194c4f556d11debec71fa7255bce1962ddc0335381c2d9071a4`.
Interval: `2026-09-26T07:29:55.508529+00:00` through
`2026-09-26T07:32:08.618895+00:00`.

The local deterministic checks also cover overlapping refresh coalescing,
placeholder creation failing with storage-full, preservation of stale files,
and successful fresh-view retry. The durability suite proves that a
follower-backed acknowledgement cannot finish drain before S3 coverage,
including reconciliation of a committed root CAS whose response was lost.

## Interrupted offline mode rollout

`qualify_mode_rollout.py --exercise-drain-faults` passed on runtime
`287a5eb1397`, runner `693b3848c96`, image `faf59e3a8b06`. The three servers
kept 1 vCPU/1 GiB limits. During enrollment only, the disposable RustFS
container received 0.25 vCPU so follower proofs could win against slower
object publication. Its normal CPU allocation was restored before faults.
Ten fleet proofs were observed; nineteen comment acknowledgements were saved.

Both selected followers of an active node log were killed. Their exit codes
were 137, the surviving server exited zero, and the canonical rollout barrier
rejected the attempt. Every fleet config remained byte-identical. Restart
recovered all nineteen comments plus the original issues and labels.

RustFS was then stopped before a second drain. All three servers exited 1;
rollout was rejected again with unchanged fleet configs. After restoring
RustFS and restarting, every acknowledged value recovered again. The final
healthy drain exited zero on all three nodes and allowed the object-mode
configuration. A new object-proven comment was acknowledged with no fleet
proofs in the new processes. Killing all three servers and deleting their
project-owned Cell volumes preserved all twenty pre/post-switch comments,
issues, and labels after restart. Two fresh readers returned sequence 38.

Receipt: `plan036-rollout-faults-2/mode-rollout-report.json`, SHA-256
`202017bbefe3f1ff0cf7dbb562a4ff138fdcf328d35d2b9d30db951196f29e2f`.
Interval: `2026-09-26T07:38:17.520942+00:00` through
`2026-09-26T07:42:20.167518+00:00`. The earlier unthrottled setup produced
only object proofs and is retained as a failed enrollment attempt in
`plan036-rollout-faults-1`; it is not a passing fault receipt.

Lost root-CAS responses and a pending fleet-only acknowledgement during drain
are separately deterministic runtime cases in `durability/proofs.rs`; they
were not injected into this HTTP/Compose run. All ten durability cases,
overlapping refresh, storage-full read-view retry, all-target LTX/runtime
Clippy, nine Python tests, formatting, layout, and documentation checks pass.
All qualification containers created by these follow-ups are stopped, with
volumes and raw evidence retained.

## CI status at closeout

The prior pushed revision's workflow-syntax check failed in
`.github/workflows/cell-property-qualification.yml:26` (unavailable `runner`
context) and `.github/workflows/cell-runtime-qualification-contract.yml:149`
(literal-dollar shell checks). Both files have identical Git blobs on this
branch and the freshly fetched `origin/main`; this work does not modify them.
Other CI jobs were still queued or running. Local proof does not turn those
checks green or qualify a merge.

## Reader discovery optimization under fixed ingress traffic

The hot path previously listed and fetched the retained node directory for
every replica query. Runtime `c9d16ebeb358344f4ab5984de2d5bbb3b5d66423` now
shares a one-second advisory membership snapshot across directory clones and
Cells, coalesces refreshes, reuses the signed owner's immutable boot identity
for exclusion, and overlaps independent control/policy loads. Expired nodes
are filtered on every selection. A failed expired refresh returns an error.
The final post-SQL authority/session checks, peer authentication, and offline
maintenance scans remain fresh. No read lease or weaker fencing was added.

The initial driver pinned six clients to node 1 and two to node 5. Fast local
owner responses dominated its primary sample: one run completed 10,224 reads
at node 1 and 44,629 at node 5. Replica traffic in that run had the opposite
proportions. Those aggregate medians do not compare identical ingress traffic.
An exploratory later restart also moved the primary to node 4; its improved
aggregate comparison is retained separately and does not establish the result
below.

The corrected driver uses eight concurrent clients, assigning three requests
to node 1 followed by one to node 5 throughout both modes. It verifies that
request mix, boots node 5 first to hold physical ownership constant between
images, and rejects owner/epoch/incarnation changes within any measured pair.
Five separate nodes retain their inspected 1 vCPU/1 GiB/no-swap limits. Each
mode lasts sixty seconds; the final three pairs alternate order. Every query
checks the acknowledged issue title/body, and replicas must include valid
receipts and use all four readers from both ingresses.

| Image / pair | Primary requests/s | Replica requests/s | Primary p50 | Replica p50 | Primary p99 | Replica p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Original / baseline | 413.41 | 98.16 | 20.67 ms | 78.63 ms | 54.99 ms | 128.10 ms |
| Optimized / 1 | 411.72 | 1,048.65 | 21.07 ms | 6.70 ms | 52.01 ms | 37.73 ms |
| Optimized / 2 | 436.35 | 875.96 | 20.52 ms | 7.28 ms | 46.21 ms | 43.15 ms |
| Optimized / 3 | 449.71 | 858.23 | 20.27 ms | 7.33 ms | 41.31 ms | 44.97 ms |

All three optimized pairs passed: replica throughput was at least 80% of
primary throughput, and replica p50/p99 were at most 120% of primary latency.
The 244,880 measured responses had zero errors, including 166,993 replica
responses. All four readers served each window. Replica throughput was
1.91–2.55 times its paired primary throughput. The local-primary path at node 5
still had a 1.86–1.91 ms median, while that ingress's replica medians were
6.90–7.57 ms. This establishes the requested fixed fleet-mix comparison;
it does not establish parity with the best-case local-primary path.

After the three pairs, pausing this fixture's RustFS returned HTTP 503
`replica_unavailable` after 5.077 seconds. Unpause restored the acknowledged
issue value, unchanged incarnation, and sequence 504 before and after the
outage. All five nodes then drained with exit code zero and no OOM kill.
Runtime tests cover schema changes during SQL, owner withdrawal, refresh
coalescing, expiry and failed discovery refresh. Sixteen concurrent selections
share five membership GETs, including owner exclusion; an independent routing
test proves policy reads start while control I/O is stalled. Focused tests,
live native RustFS, minimal-feature build, all-target runtime Clippy, format,
layout, and documentation checks pass. The change adds 80 net Rust production
lines, with no dependency, manifest, or lockfile changes.

Raw receipts remain under `$HOME/.codex/cell-issue-fleet/plan036-sparse-cost-1`:

- Baseline: `reader-perf-fixed-before-3.json`, SHA-256
  `76ba5573a01fc0ad1d22c45c0a58a6d88ae4102176b7e0368a31f295813c7aa1`.
  Runtime `287a5eb1397c768f00f753bfc592c58236738a59`, runner
  `c9d16ebeb358344f4ab5984de2d5bbb3b5d66423`, image
  `sha256:faf59e3a8b06bea7b4b57473428642a7677fd69b454ce3580150558d0fea381f`.
- Final: `reader-perf-final-1.json`, SHA-256
  `d5dc8e5ed6c1f77ca075457665a9ea54cfa86a40e0e2009e76657534dc0ec131`.
  Runtime `c9d16ebeb358344f4ab5984de2d5bbb3b5d66423`, runner
  `ebc72e7e4e448667940fb9fb64c791bb1df3e5a3`, image
  `sha256:1b3e10a60a65d8f8ada1079f7baf61ddfcb76497cc9060c98cbe73a36fd8ed5e`.
  Interval `2026-09-26T15:26:49.114114+00:00` through
  `2026-09-26T15:33:21.056970+00:00`.

## Reconciliation failure isolation follow-up — 2026-09-26

The owner reconciler now continues after a Cell's policy/provider error and
advances its cursor before awaiting that Cell. A thirty-second batch deadline
and cancellation branch prevent provider or peer stalls from holding node
shutdown or the same cursor indefinitely. Reader hints use sixteen concurrent
attempts with a thirty-second fanout deadline. Each dispatched attempt reloads
the exact signed boot session and uses a fresh authorization timestamp.

The corrupt-policy, stalled-peer, and stale-advertisement regressions all failed
against the preceding implementation and passed with the fix. A fourth test
proves that an interrupted batch resumes at the next Cell. All ten router tests
passed, including existing rebalance, restore, and takeover paths. The ignored
`rustfs_public_collaboration_reaches_remote_owner_and_publishes_ltx` test passed
against the existing local RustFS with an isolated prefix: public HTTP, private
mTLS, explicit replica reads, target-count changes, and restored values after
takeover. These changes affect reconciliation, not the measured query path;
the performance figures above remain tied to their recorded images.

At the preceding PR head, both `Multi-crate guardrails` and `Signed receipt and
canonical contract checks` fail the same architecture policy: the existing
`crab-cell-app` dev dependency on `crab-cell-host` is absent from its dependency
inventory. Both the manifest and `check-architecture-gates.py` are unchanged
from `origin/main`. This follow-up does not change the policy inventory to
suppress that failure. Review readiness does not imply green merge gates.

## Scope still open

The container runs do not establish complete per-query S3 costs, production
hot Cell throughput, per-reader peak resources, all release-fault
interleavings, platform rolling upgrades, or 1k/5k/10k Cell admission. The
optimized five-node result qualifies its fixed ingress traffic mix; the older
3/5/10/20-node measurements remain bound to their earlier images. The optimized
image has not been performance-qualified at twenty nodes or across hosts.
Protected S3 and multi-host release gates remain outside the requested local
RustFS execution scope. See [Plan 036](../../../../../advisor-plans/036-cell-read-replicas-and-fenced-promotion.md).
