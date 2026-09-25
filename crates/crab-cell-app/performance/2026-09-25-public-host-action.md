# Reference application public-host action measurements — 2026-09-25

Two release-profile runs used the same three-`CellNode` reference fixture with
100 serial actions in each lane. The source was commit `3b8d3b3614c` plus the
uncommitted application-boundary changes in this worktree. The host was an
Apple M2 Max, Darwin arm64, with Rust 1.97.0. SQLite used temporary local
files; the object store was in memory. Signed peer requests crossed loopback
TCP. The forwarded lane went through a second gateway peer before reaching the
owner. The two lanes ran in sequence against the same SQL Cell.

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-8bc8 \
  cargo test -p crab-cell-app --test reference_application \
  public_host::reference_public_host_action_performance \
  --release --locked -- --ignored --nocapture
```

Each action prepared the generated typed command, waited for its published
receipt, and verified the resulting count with a generated typed query at that
receipt. `execute → durable ack` starts after preparation and ends when the
command returns its receipt. It includes command execution and, on the
forwarded lane, network transport. `Object durability proof wait` is the
runtime telemetry from post-commit submission until the object proof; it
includes publication queue time and is separate from transport and query time.
Throughput below is verified full actions per second for the whole lane.

| Measure | Run | Throughput | p50 ms | p95 ms | p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Local verified action | 1 | 811.85 | 0.736 | 1.030 | 16.286 |
| Local verified action | 2 | 827.39 | 0.720 | 0.885 | 16.492 |
| Local execute → durable ack | 1 | — | 0.700 | 0.997 | 16.251 |
| Local execute → durable ack | 2 | — | 0.688 | 0.855 | 16.453 |
| Local object durability proof wait | 1 | — | 0.195 | 0.317 | 15.779 |
| Local object durability proof wait | 2 | — | 0.194 | 0.267 | 15.935 |
| Forwarded verified action | 1 | 347.99 | 2.369 | 3.049 | 15.065 |
| Forwarded verified action | 2 | 350.52 | 2.348 | 3.137 | 15.665 |
| Forwarded execute → durable ack | 1 | — | 1.165 | 1.491 | 13.815 |
| Forwarded execute → durable ack | 2 | — | 1.147 | 1.410 | 14.445 |
| Forwarded object durability proof wait | 1 | — | 0.234 | 0.338 | 12.827 |
| Forwarded object durability proof wait | 2 | — | 0.227 | 0.331 | 13.489 |

Owner fencing, authority takeover, exact-root restore, and the first verified
typed query took **9.682 ms** in run 1 and **8.695 ms** in run 2. Those are one
recovery sample per run, so they cannot define a recovery percentile or SLO.

The runs did not include cloud storage, a real node-log durability provider,
dynamic placement, process failure, concurrent load, or large Cell counts.
They do not set a supported capacity, throughput, or recovery limit.
