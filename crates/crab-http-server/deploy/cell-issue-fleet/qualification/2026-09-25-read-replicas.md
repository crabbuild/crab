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

## Scope still open

The container runs do not measure per-query S3 calls or refresh bytes, sustained
hot Cell throughput, peak resources, retention/release fault combinations, or
1k/5k/10k Cell admission. Sparse readers remain unimplemented; distribution
under uneven load across multiple ingress nodes remains unqualified.
Protected S3 and multi-host release gates remain outside the requested local
RustFS execution scope. See [Plan 036](../../../../../advisor-plans/036-cell-read-replicas-and-fenced-promotion.md).
