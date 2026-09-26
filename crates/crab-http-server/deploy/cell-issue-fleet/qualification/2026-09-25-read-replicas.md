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

## Scope still open

This run does not measure per-query S3 calls or refresh bytes, sustained hot
Cell throughput, peak resources, retention/release fault combinations, or
1k/5k/10k Cell admission. It also does not complete sparse readers, generic
application-client replica policy, or routing by measured in-flight load.
Protected S3 and multi-host release gates remain outside the requested local
RustFS execution scope. See [Plan 036](../../../../../advisor-plans/036-cell-read-replicas-and-fenced-promotion.md).
