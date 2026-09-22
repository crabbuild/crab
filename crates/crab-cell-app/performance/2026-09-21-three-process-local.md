# Cell three-process fleet performance — 2026-09-21

Measured code: `a87ac6c2b20`.

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
| SQL order insert and read | 1 | 66.27 | 11.825 | 30.225 | 53.176 | 78.881 |
| SQL order insert and read | 2 | 69.41 | 12.003 | 24.490 | 60.409 | 61.126 |
| KV cart put and get | 1 | 88.17 | 9.450 | 18.191 | 55.173 | 59.934 |
| KV cart put and get | 2 | 89.56 | 9.250 | 17.136 | 52.264 | 53.874 |
| Blob attachment upload and read, 32 KiB | 1 | 29.50 | 29.277 | 65.929 | 78.255 | 79.361 |
| Blob attachment upload and read, 32 KiB | 2 | 31.37 | 27.980 | 61.950 | 76.333 | 78.234 |
| Queue notification send, claim, acknowledge | 1 | 31.52 | 26.549 | 72.309 | 89.642 | 100.055 |
| Queue notification send, claim, acknowledge | 2 | 33.16 | 26.395 | 62.790 | 75.801 | 80.204 |
| Workflow start, native activity, terminal read | 1 | 28.88 | 30.384 | 64.314 | 96.724 | 97.024 |
| Workflow start, native activity, terminal read | 2 | 30.64 | 30.138 | 61.203 | 73.995 | 78.475 |
| Cron schedule, tick, effect delivery, SQL read | 1 | 19.65 | 48.196 | 102.352 | 115.194 | 130.834 |
| Cron schedule, tick, effect delivery, SQL read | 2 | 20.69 | 45.695 | 92.352 | 100.961 | 101.188 |
| **Fleet, all six actions** | **1** | **117.86** | **24.769** | **73.771** | **102.293** | **130.834** |
| **Fleet, all six actions** | **2** | **124.16** | **24.549** | **65.847** | **89.620** | **101.188** |

The two runs vary under uncontrolled desktop load. The process test proves
separate owner processes, signed peer dispatch, shared authority, and
fleet-level result verification. It does not measure separate machines,
inter-host network latency, mTLS, product ingress or dynamic placement, a cloud
object provider, follower durability, node loss, or a representative sustained
traffic mix. The in-process Workflow activity handler returns its input. The
three-runtime report uses in-memory storage, so its numbers do not isolate the
cost of process separation.
