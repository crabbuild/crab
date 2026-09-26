# RustFS action attribution and restart failure — 2026-09-26

The three-node load phase completed all 300 create/read pairs without retries.
The qualification **failed** when restarting the killed owner: its process
refused startup because usable disk was below the required 20 GiB. The runner
stopped before the 5/10/20-node stages. These measurements describe the completed
load phase; they do not qualify recovery, throughput capacity, or the latest
cache-construction change.

## Source and workload

Both server and clean generator source were
`e50055c48bbf5fd3b039d290e5afcb791d326422`. The image came from passing
[ARM64 image/Compose run 36239430827](https://github.com/crabbuild/crab/actions/runs/36239430827).
The import verified the archive checksum, source label and platform.

| Input | Value |
| --- | --- |
| Archive SHA-256 | `83386343d111dd7c22ddbeb52aa29c79907113d54bd0f73827109dcda99e71b5` |
| OCI config digest | `sha256:def4032d0655eb6be8040ba21e3f306e57a6dca66c135964dc1e492202ea2ea2` |
| Imported image / OCI manifest digest | `sha256:fab4c3f79c0f03e23f1517e27eb4444480e49d7c555c4c59775eb763c7f1f8b0` |
| Platform / Docker context | `linux/arm64` / `colima` |
| Shared VM | 8 CPUs; 16,732,602,368 memory bytes |
| Each Cell node | 1 CPU limit; 1,073,741,824 memory bytes; no swap |
| Load | 20 Cells; 5 scheduled create/read pairs/s; 60 seconds; at most 64 pairs in flight |
| Observed concurrency | Peak 1 pair in flight |
| Tracing | `info,crab_cell_runtime::action=debug,crab_http_server::action=debug` |

The pinned RustFS, Caddy ingress, mTLS transport and per-node volumes are the
ones rendered by [render.py](../render.py). Nodes share the VM's disk and
network namespace. The resource profile supplies separate processes and
enforced CPU/memory ceilings; it does not supply independent disks or hosts.

## Completed load phase

All 600 HTTP operations succeeded. Every acknowledged write joined to exactly
one runtime response and commit sequence; all 300 winners used object proof.
There were 178 forwarded writes. Actual write owners handled 105, 105 and 90
writes; entry nodes handled 200, 197 and 203 total HTTP operations. All 300
acknowledged issues were verified before the recovery fault. Every Cell's
published root advanced. The first post-load observation found zero uncovered
bytes on all nodes; collecting it took 0.589 seconds.

Percentiles use the nearest-rank method on individual action samples.

| Observation | Samples | p50 ms | p95 ms | p99 ms |
| --- | ---: | ---: | ---: | ---: |
| HTTP write, all routes | 300 | 20.738 | 35.213 | 38.973 |
| HTTP read | 300 | 6.561 | 11.445 | 21.322 |
| HTTP write, local owner | 122 | 12.152 | 23.188 | 28.323 |
| HTTP write, forwarded | 178 | 24.556 | 37.750 | 43.226 |
| Actor queue | 300 | 0.006 | 0.013 | 0.017 |
| SQL worker queue | 300 | 0.024 | 0.047 | 0.068 |
| SQL worker execution, including capture | 300 | 2.087 | 5.740 | 8.733 |
| Durability proof task wait | 300 | 6.115 | 17.157 | 20.073 |
| Final worker confirmation | 300 | 0.145 | 0.522 | 0.757 |

These intervals overlap. Capture is inside worker execution, and proof wait
excludes submission work before the proof task starts. Do not sum percentile
columns. The local and forwarded populations are observational groups, not a
controlled measurement of peer-hop cost.

The same-action difference between HTTP response-readiness time and typed
invocation time had local p50/p95 of 0.563/1.081 ms and forwarded p50/p95 of
7.482/14.065 ms. This difference includes work both before and after invocation:
authentication, routing, command preparation, response enrichment and other
HTTP work. It cannot identify any one of these as the cause. The issue create
handler performs a label-catalog query after the mutation; query and routing
spans are required to attribute this portion.

Twenty of the 300 captures included checkpoint work. Their capture p50/p95
was 3.881/7.696 ms, compared with 0.300/0.510 ms for the other 280. The slowest
HTTP write was 46.380 ms, including 10.677 ms of worker execution and 7.696 ms
of capture, of which 3.229 ms was checkpoint time. This establishes checkpoint
participation in those actions, not its isolated causal contribution or a
long-run tail distribution.

## Restart failure and evidence limitation

The node restart exited with:

```text
Error: Config("Cell runtime requires at least 20 GiB usable local disk")
```

[Startup budgeting](../../../src/server.rs) subtracts the larger of 10 GiB or
20% of available disk before requiring 20 GiB usable. The example caps each
node's measured disk at 30 GiB; its named volumes share one filesystem and
have no separate disk quota. A later sample on a surviving node's actual Cell
mount reported 32,184,803,328 available bytes, below 30 GiB (32,212,254,720).
This later sample supports the startup error; it is not a measurement at the
exact failure instant and does not attribute disk consumption to this load.

The restart runs in `recover_owner`'s `finally` block. Its exception overrides
any pending successful return or earlier recovery exception. Consequently,
`owner_loss` is absent from the report: neither recovery time nor complete
post-fault verification can be certified from this receipt. Preserve separate
takeover, acknowledgement-verification and restart results in the next harness
change, and make an unsuccessful restart keep the overall run failed.

Before a new scale run, provision shared disk headroom, record the actual Cell
mount's available bytes, and preflight the existing production budget. Keep
the production reserve intact. Disk-pressure qualification needs independently
bounded storage or an explicitly shared-resource scenario, plus a loss-of-local-
data test. These steps belong to the qualification boundary.

## Retained evidence and reproduction

Raw files are under `$HOME/.codex/cell-issue-fleet/ci-36239430827/`:
`report.json`, `load-3-stage.json`, its `.samples.jsonl` and `.nodes.jsonl`
companions, and `load-3-stage.traces/` containing per-node logs and joined
`actions.jsonl`. The source and import receipts, host envelope, qualifier log,
derived `action-phase-summary.json`, and `restart-diagnostic.log` are under
`$HOME/Workspace/crabbuild-target/crab-8bc8/ci-36239430827/`.

The latter directory also retains `fleet-action-audit-evidence.tar.gz`, with
SHA-256 `0b31a34a43741c3375b1283c8b37860fd8ad7aa2835d946541da89ced3a83d35`.
The archive contains the raw samples, traces, reports and provenance above;
configuration and credential files are excluded.

Use the [CI import runbook](../README.md#run-a-ci-qualified-linux-image) with
the exact source above and a fresh project/state directory. The qualification
arguments were `--skip-build --load-stages --cells 20 --load-rate 5
--load-duration 60 --load-max-in-flight 64 --gateway-port 18880
--node-port-base 18900 --rustfs-port 19020`, with `DOCKER_CONTEXT=colima`.
Do not rerun over the retained report paths. Five pairs/s is a latency probe;
sustained rate sweeps, update/skew workloads, follower-only acknowledged-tail
faults and public application primitive qualification remain open.
