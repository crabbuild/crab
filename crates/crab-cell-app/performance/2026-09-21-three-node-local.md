# Cell three-runtime fleet performance — 2026-09-21

Measured code: `b6906e0a2b2`.

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
| SQL order insert and read | 1 | 171.70 | 4.595 | 13.565 | 19.282 | 21.344 |
| SQL order insert and read | 2 | 159.33 | 4.824 | 14.368 | 23.943 | 24.995 |
| KV cart put and get | 1 | 250.95 | 3.097 | 7.599 | 20.286 | 22.433 |
| KV cart put and get | 2 | 198.40 | 3.496 | 9.379 | 38.341 | 45.279 |
| Blob attachment upload and read, 32 KiB | 1 | 78.91 | 9.727 | 30.759 | 50.553 | 53.233 |
| Blob attachment upload and read, 32 KiB | 2 | 68.71 | 11.131 | 34.768 | 40.280 | 46.452 |
| Queue notification send, claim, acknowledge | 1 | 84.73 | 8.781 | 30.029 | 35.392 | 37.575 |
| Queue notification send, claim, acknowledge | 2 | 72.71 | 10.355 | 34.490 | 43.590 | 44.552 |
| Workflow start, native activity, terminal read | 1 | 71.82 | 11.969 | 27.440 | 37.836 | 41.092 |
| Workflow start, native activity, terminal read | 2 | 61.83 | 13.395 | 36.812 | 44.896 | 47.363 |
| Cron schedule, tick, effect delivery, SQL read | 1 | 37.65 | 22.602 | 45.645 | 60.671 | 66.631 |
| Cron schedule, tick, effect delivery, SQL read | 2 | 30.67 | 28.694 | 60.528 | 78.548 | 87.508 |
| **Fleet, all six actions** | **1** | **225.88** | **8.859** | **33.713** | **48.450** | **66.631** |
| **Fleet, all six actions** | **2** | **183.99** | **10.293** | **40.280** | **56.911** | **87.508** |

These measurements verify three independent runtime owners and the signed peer
protocol over the local TCP stack. They do not measure three processes or
machines, mTLS, product ingress and dynamic routing, real network delay, cloud
object storage, follower durability, ownership movement, or failure recovery.
The Workflow activity handler returns its input in-process. Production fleet
capacity and latency require a deployed three-node qualification with a real
provider and representative client concurrency.
