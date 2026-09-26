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

## Scope still open

The container runs do not establish complete per-query S3 costs, sustained
hot Cell throughput, peak resources, retention/release fault combinations, or
1k/5k/10k Cell admission. Distribution under uneven load across multiple
ingress nodes remains unqualified. Sparse-reader measurements above show
balanced distribution but lower throughput than owner reads in this workload.
Protected S3 and multi-host release gates remain outside the requested local
RustFS execution scope. See [Plan 036](../../../../../advisor-plans/036-cell-read-replicas-and-fenced-promotion.md).
