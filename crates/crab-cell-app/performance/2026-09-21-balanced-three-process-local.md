# Cell balanced three-process fleet performance — 2026-09-21

Measured code: `752ad2c0067`.

The load generator and load balancer ran in one process, with three separate
Cell owner processes on one Apple M2 Max host (Darwin 25.5.0 arm64, Rust
1.97.0). Each owner had its own node session, SQLite worker pool, and database
directory. Seven Cells were placed across the owners as in the
[direct-owner report](2026-09-21-three-process-local.md). All processes shared
the test-only filesystem CAS object store.

The load generator sent signed Cell requests to a loopback TCP balancer. The
balancer forwarded each request to one of the three owners in round-robin
order without changing the signed bytes. Every owner verified the request and
served a locally owned Cell or forwarded it once over loopback TCP to the
owner in a static Cell-ID map. The owner then dispatched through the Cell actor.
The client-to-balancer, balancer-to-entry, and any entry-to-owner round trips
are inside the action timer. Every peer request opens a new TCP connection.
The balancer ran as a task in the load-generator process, not as an external
service.

The six actions in `PERFORMANCE.md` ran as concurrent lanes, with 100 serial
actions per lane. Each action included its visible result check, including SQL
read-back for Cron effect delivery. Cron included the intentional 6 ms due-time
wait. Bootstrap and shutdown were outside the timer. Fleet throughput divides
600 verified actions by the wall time until all lanes finish; fleet latency
percentiles pool all 600 samples. At most six actions were in flight, so this
finite workload is not a saturation or production capacity measurement.

Run from the repository root with an external target directory unique to the
checkout:

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-89be5c6d \
  cargo test -p crab-cell-app --test reference_application \
  process_performance::reference_balanced_three_process_fleet_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

| Verified action | Run | actions/s | p50 ms | p95 ms | p99 ms | max ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| SQL order insert and read | 1 | 41.77 | 14.886 | 57.259 | 210.677 | 245.825 |
| SQL order insert and read | 2 | 52.85 | 16.102 | 36.387 | 59.547 | 71.671 |
| KV cart put and get | 1 | 49.25 | 12.244 | 40.209 | 228.303 | 355.001 |
| KV cart put and get | 2 | 67.87 | 12.582 | 24.601 | 49.140 | 70.371 |
| Blob attachment upload and read, 32 KiB | 1 | 22.84 | 33.561 | 84.616 | 234.761 | 306.165 |
| Blob attachment upload and read, 32 KiB | 2 | 22.86 | 36.549 | 77.706 | 92.300 | 454.294 |
| Queue notification send, claim, acknowledge | 1 | 23.66 | 32.874 | 74.543 | 241.752 | 299.029 |
| Queue notification send, claim, acknowledge | 2 | 25.91 | 34.079 | 68.093 | 82.974 | 112.842 |
| Workflow start, native activity, terminal read | 1 | 21.96 | 36.825 | 88.341 | 256.105 | 268.218 |
| Workflow start, native activity, terminal read | 2 | 22.57 | 35.291 | 82.797 | 111.565 | 415.548 |
| Cron schedule, tick, effect delivery, SQL read | 1 | 15.67 | 49.818 | 137.387 | 327.909 | 358.473 |
| Cron schedule, tick, effect delivery, SQL read | 2 | 15.66 | 56.400 | 113.502 | 160.807 | 431.613 |
| **Fleet, all six actions** | **1** | **94.01** | **29.647** | **94.869** | **256.105** | **358.473** |
| **Fleet, all six actions** | **2** | **93.94** | **30.429** | **83.836** | **114.228** | **454.294** |

Each run sent 5,000 peer requests through the balancer. Entry selection was
1,667 / 1,667 / 1,666 requests across nodes 0 / 1 / 2. The receiving nodes
forwarded 831 / 1,542 / 1,016 requests in run 1 and 822 / 1,533 / 1,019 in
run 2. Every node served local requests and forwarded remote requests. The
different per-node forwarding counts reflect the unequal seven-Cell placement
and the primitive mix, not uneven balancer selection.

Run 1 had a substantially higher latency tail than run 2 under uncontrolled
desktop load. A nearby direct-owner run was also slower than these balanced
runs, so these observations cannot isolate load-balancer or forwarding cost.
This test does not measure separate machines, an external balancer, provider
network latency, product ingress, mTLS, dynamic owner discovery or placement,
follower durability, node loss, or millions of instantiated Cells. The Workflow
activity handler remains the in-process reference echo handler. A deployed
three-machine test and a separate high-cardinality control-plane test are
needed to qualify those claims.
