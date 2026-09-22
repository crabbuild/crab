# Cell three-runtime fleet performance — 2026-09-21

Measured code: `a87ac6c2b20`.

This is a local three-runtime topology on one Apple M2 Max host (macOS Darwin
25.5.0 arm64, Rust 1.97.0). Each runtime has a distinct node session, SQLite
worker pool, and local database files. All three share one in-memory object
store for Cell authority, publication, and Blob artifacts. A load generator
uses the compiled typed application and signed private peer requests. Static
Cell-ID routing sends every Cell request over loopback TCP to the owning runtime;
the receiver verifies the signature and dispatches through its local Cell actor.
Every peer request opens a new TCP connection. Cron effect delivery crosses from
the Cron runtime to the SQL runtime through the same peer path.

| Runtime | Owned Cells |
| --- | --- |
| 0 | SQL, Queue, Workflow |
| 1 | KV, Queue dead letter |
| 2 | Blob, Cron |

The benchmark runs six concurrent lanes, one per primitive. Each lane performs
100 serial, fully verified user actions from `PERFORMANCE.md`. A complete action
includes its write, follow-up read or terminal/delivery check, and all peer
round trips. The timer excludes seven-Cell bootstrap and shutdown. Cron actions
include the intentional 6 ms due-time wait. Fleet throughput is 600 completed
actions divided by the wall time from starting all lanes until the last lane
finishes. The fleet latency percentiles pool all 600 action samples. The lanes
finish at different times, so this is a finite equal-mix workload at at most
six concurrent actions, not a fleet saturation limit.

Run from the repository root with an external target directory unique to this
checkout:

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-89be5c6d \
  cargo test -p crab-cell-app --test reference_application \
  performance::reference_three_node_fleet_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

| Verified action | Run | actions/s | p50 ms | p95 ms | p99 ms | max ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| SQL order insert and read | 1 | 172.15 | 4.314 | 13.881 | 22.203 | 30.736 |
| SQL order insert and read | 2 | 158.07 | 4.886 | 14.215 | 27.095 | 31.987 |
| KV cart put and get | 1 | 231.45 | 3.024 | 7.788 | 28.672 | 35.787 |
| KV cart put and get | 2 | 207.66 | 3.325 | 10.209 | 22.365 | 33.185 |
| Blob attachment upload and read, 32 KiB | 1 | 77.31 | 9.293 | 33.081 | 46.598 | 46.978 |
| Blob attachment upload and read, 32 KiB | 2 | 71.87 | 9.984 | 32.401 | 44.580 | 46.666 |
| Queue notification send, claim, acknowledge | 1 | 82.20 | 8.671 | 30.963 | 35.811 | 38.561 |
| Queue notification send, claim, acknowledge | 2 | 75.22 | 9.510 | 28.723 | 39.637 | 43.682 |
| Workflow start, native activity, terminal read | 1 | 68.71 | 10.871 | 35.750 | 48.053 | 49.629 |
| Workflow start, native activity, terminal read | 2 | 63.53 | 11.962 | 32.491 | 50.760 | 52.046 |
| Cron schedule, tick, effect delivery, SQL read | 1 | 37.41 | 23.270 | 45.311 | 59.227 | 67.312 |
| Cron schedule, tick, effect delivery, SQL read | 2 | 35.02 | 25.811 | 49.500 | 64.387 | 66.511 |
| **Fleet, all six actions** | **1** | **224.41** | **8.763** | **35.718** | **46.978** | **67.312** |
| **Fleet, all six actions** | **2** | **210.10** | **9.394** | **36.952** | **50.760** | **66.511** |

These measurements verify three independent runtime owners and the signed peer
protocol over the local TCP stack. They do not measure three processes or
machines, mTLS, product ingress and dynamic routing, real network delay, cloud
object storage, follower durability, ownership movement, or failure recovery.
The Workflow activity handler returns its input in-process. Production fleet
capacity and latency require a deployed three-node qualification with a real
provider and representative client concurrency.
