# Cell three-process fleet performance — 2026-09-21

Measured code: `da3479be91b`.

The load generator and three Cell owners ran as four OS processes on one Apple
M2 Max host (macOS Darwin 25.5.0 arm64, Rust 1.97.0). Each owner had a distinct
node session, SQLite worker pool, and database directory. The seven Cells were
placed across owners as follows:

| Owner process | Cells |
| --- | --- |
| 0 | SQL, Queue, Workflow |
| 1 | KV, Queue dead letter |
| 2 | Blob, Cron |

The processes shared a test-only filesystem object store. Its conditional
control-record update uses a cross-process file lock; object writes use the
local filesystem. The load generator routed each typed Cell request by Cell ID
over loopback TCP. Each owner verified the signed peer request, resolved only
its own Cells, and dispatched through the Cell actor. Every request opened a
new TCP connection. Cron effect delivery crossed from process 2 to SQL on
process 0. Blob part bytes used the shared object store. The load generator
started the six verified actions in `PERFORMANCE.md` as concurrent lanes, with
100 serial actions per lane. The timer excluded bootstrapping and graceful
shutdown; each action included its follow-up read or durable outcome check.
Cron included the intentional 6 ms due-time wait.

Fleet throughput is 600 verified actions divided by the wall time until all
lanes finish. Fleet latency percentiles pool all 600 end-to-end action samples.
The lanes finish at different times. This is a finite equal-mix workload with
at most six actions in flight, not a saturation or production capacity limit.

Run from the repository root with a target directory unique to this checkout:

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-89be5c6d \
  cargo test -p crab-cell-app --test reference_application \
  process_performance::reference_three_process_fleet_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

| Verified action | Run | actions/s | p50 ms | p95 ms | p99 ms | max ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| SQL order insert and read | 1 | 51.08 | 10.774 | 28.765 | 62.685 | 630.527 |
| SQL order insert and read | 2 | 67.78 | 11.224 | 29.322 | 76.396 | 84.300 |
| KV cart put and get | 1 | 58.18 | 9.700 | 20.331 | 51.165 | 603.337 |
| KV cart put and get | 2 | 81.48 | 9.575 | 24.364 | 69.026 | 73.429 |
| Blob attachment upload and read, 32 KiB | 1 | 27.26 | 27.933 | 64.754 | 109.453 | 573.580 |
| Blob attachment upload and read, 32 KiB | 2 | 29.85 | 27.161 | 77.298 | 104.404 | 105.117 |
| Queue notification send, claim, acknowledge | 1 | 28.90 | 24.998 | 60.744 | 71.109 | 581.684 |
| Queue notification send, claim, acknowledge | 2 | 32.01 | 25.620 | 73.569 | 84.210 | 97.581 |
| Workflow start, native activity, terminal read | 1 | 27.05 | 28.676 | 62.576 | 79.038 | 603.593 |
| Workflow start, native activity, terminal read | 2 | 29.42 | 28.088 | 74.752 | 82.610 | 86.022 |
| Cron schedule, tick, effect delivery, SQL read | 1 | 19.30 | 43.739 | 93.350 | 122.960 | 675.891 |
| Cron schedule, tick, effect delivery, SQL read | 2 | 19.97 | 48.664 | 101.352 | 138.467 | 139.929 |
| **Fleet, all six actions** | **1** | **115.81** | **22.808** | **68.027** | **122.960** | **675.891** |
| **Fleet, all six actions** | **2** | **119.78** | **23.727** | **75.122** | **103.650** | **139.929** |

One run had several 0.6–0.7 s maximum latencies. These are observed outliers;
this uncontrolled desktop run does not establish their cause. The process test
proves separate owner processes, signed peer dispatch, shared authority, and
fleet-level result verification. It does not measure separate machines,
inter-host network latency, mTLS, product ingress or dynamic placement, a cloud
object provider, follower durability, node loss, or a representative sustained
traffic mix. The in-process Workflow activity handler returns its input. The
three-runtime report uses in-memory storage, so its numbers do not isolate the
cost of process separation.
