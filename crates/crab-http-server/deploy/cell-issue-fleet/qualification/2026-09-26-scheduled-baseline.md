# Scheduled RustFS fleet measurements — 2026-09-26

This run used the same 20 Cells at 3, 5, 10, and 20 nodes, followed by a
second 20-node measurement. Each phase scheduled five create/read pairs per
second for 60 seconds, with at most 64 pairs in flight. All five phases
completed their 300 pairs: 1,500 successful pairs, 3,000 successful HTTP
operations, no retries, and no dropped or late arrivals. This offered rate
does not measure saturation throughput.

## Provenance and resources

Both the server and clean load-generator checkout were
`c12b41ef63810974ca0827931d48671884d664ad`. The native ARM64 image came from
passing [CI run 36227844137](https://github.com/crabbuild/crab/actions/runs/36227844137).
Its downloaded artifact checksum was verified before import:

| Identity | Value |
| --- | --- |
| Image archive SHA-256 | `ee7ca95eefc9519a0a0fecb571e984f2095cf6360bf6dafbc3a23a9c421f0978` |
| CI image ID / OCI config digest | `sha256:138b51e2cc1b72b5cda5f6236eb3c95c0fa96d726a3d98cce797b35c0cef8108` |
| Colima image ID / OCI manifest digest | `sha256:e7d73e7975d83d1029f02b05217c3ea8045a9370fad78df799722a03c51ec872` |
| Server platform | `linux/arm64` |
| Docker context / version | `colima` / `29.5.2` |
| Shared VM capacity | 8 CPUs; 16,732,602,368 memory bytes |
| Each node's enforced limits | 1 CPU; 1,073,741,824 memory bytes; no swap |

The qualifier pinned every server container to the imported image ID. All
stages used the pinned RustFS image from [render.py](../render.py), persistent
RustFS and per-node Cell volumes, Caddy round-robin ingress, and the same
resource/metrics observer. These are separate processes on one VM with a
shared network namespace. Twenty CPU limits do not supply twenty independent
CPUs, hosts, or failure domains.

## Measurements

Each latency column contains 300 successful operation samples. Recovery is
one observation per phase, measured after arrivals and publication drain.

| Nodes | Write p50 / p95 / p99 (ms) | Read p50 / p95 / p99 (ms) | Entry requests per node | Peak pairs in flight | Owner recovery (s) |
| ---: | --- | --- | --- | ---: | ---: |
| 3 | 24.797 / 40.590 / 43.942 | 8.285 / 14.579 / 17.382 | 199–201 | 1 | 7.780 |
| 5 | 24.222 / 134.523 / 182.667 | 10.295 / 41.634 / 54.714 | 119–121 | 2 | 10.732 |
| 10 | 28.705 / 50.232 / 75.229 | 10.857 / 20.892 / 38.717 | 59–61 | 1 | 8.841 |
| 20 | 43.092 / 346.966 / 880.731 | 24.093 / 131.714 / 408.945 | 29–32 | 9 | 10.717 |
| 20, repeat | 26.383 / 68.265 / 81.236 | 18.822 / 57.922 / 63.740 | 29–32 | 1 | 9.732 |

Every phase observed newer object roots and zero uncovered publication bytes
in its first post-load snapshot. Collecting that snapshot took 0.51–3.60
seconds; it does not measure the exact moment publication became idle. The
new owner served the selected last acknowledged issue with the same published
root in every recovery. The repeat retained the existing data and owners after
the preceding scale/failure sequence; it was not a reset to an identical
database or placement state.

## What remains unproven

- **Tail latency is variable.** Twenty-node write p95 changed from 347 to
  68 ms on the repeat, without a server code change. The runs do not isolate
  join activity, placement, metadata/cache state, publication, or shared-host
  interference. Treat each as a hypothesis requiring phase measurements.
- **Execution is not evenly distributed.** Functional-stage owner snapshots
  used 3, 4, 8, and 8 distinct owners respectively. In the first 20-node timed
  phase, command-response counters increased on nine nodes and stayed flat on
  eleven. Balanced gateway entries alone do not establish balanced execution.
  These asynchronous snapshots do not bind every action to an execution owner.
- **The fast follower path is barely represented.** Differences between the
  first and last metrics snapshots counted 361/394/375 object-backed responses
  and no fleet-backed responses at 3/5/10 nodes. The first 20-node phase counted
  466 object-backed and one fleet-backed response; the repeat counted 285 and
  one. Counters include runtime work beyond the issue POST and cover partial,
  non-atomic observation windows. They cannot be subtracted from HTTP timings
  or used as per-action durability attribution.
- **The faults are post-drain.** This generator revision checks immediate
  readback and the selected latest result after takeover. It predates the
  all-acknowledgement verification in `9ec6da5176e` and buffered compaction in
  `c6870fd6a88`. It does not qualify those changes, an unpublished follower
  tail during arrivals, loss of owner local data, or recovery percentiles.
- **Application and capacity gates remain.** Entity, Shard, Workflow and
  read-model actions through public handles, sustained saturation/skew curves,
  current-image runs, and independent-host faults still need proof.

## Retained evidence and reproduction

Raw reports, pair samples, and node metrics are in
`$HOME/.codex/cell-issue-fleet/ci-36227844137/`. Environment, import receipts,
logs, and the compressed evidence copy are in
`$HOME/Workspace/crabbuild-target/crab-8bc8/ci-36227844137/`.
The archive `fleet-baseline-evidence.tar.gz` contains all five reports and
their raw samples, the scale report, host envelope, and image receipts. Its
SHA-256 is
`69bb69627e48965017637f68cffdef41e68f6ea5626191adb9993c76d199ac9c`.

Use the [CI image import runbook](../README.md#run-a-ci-qualified-linux-image)
with the passing run above and a fresh project/state directory. The scale
command used `--cells 20 --load-stages --load-rate 5 --load-duration 60
--load-max-in-flight 64`, gateway port `18880`, node port base `18900`, and
RustFS port `19020`. The repeat used `load.py --nodes 20 --cells 20 --rate 5
--duration 60 --max-in-flight 64` against that same stack. Set
`DOCKER_CONTEXT=colima` on each invocation to select this host explicitly.
