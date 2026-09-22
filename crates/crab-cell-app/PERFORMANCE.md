# Cell primitive end-to-end performance

The ignored `reference_primitive_end_to_end_performance` test measures complete,
serial user actions through a compiled `crab-cell-app` handle, the local Cell
router, SQLite actors, LTX publication, and read-back verification. Each result
is one verified action, not one primitive method call. The benchmark creates
seven Cells before the timer starts and uses a temporary local directory plus
the in-memory object store. Cron effect delivery traverses signed peer request
encoding, verification, dispatch, and the destination Cell through a loopback
transport. No HTTP listener, network, cloud object store, failover, or concurrent
client load is measured.
The Workflow handler is the reference application's in-process echo handler;
its result verifies durable activity completion but does not simulate an
external service call.

| Result | Timed action and observed side effect |
| --- | --- |
| `sql_order_insert_read` | Insert an order with a parameterized statement; read the total at its commit receipt. |
| `kv_cart_put_get` | Save a cart value; read the exact bytes at its commit receipt. |
| `blob_attachment_upload_read_32k` | Begin, upload and commit a 32 KiB binary attachment; read and compare all bytes. |
| `queue_notification_send_claim_ack` | Send a notification, claim and validate its lease, then acknowledge it. |
| `workflow_fulfillment_activity` | Start a fulfillment workflow, execute its registered native activity, then read terminal state. |
| `cron_invoice_schedule_deliver` | Register a due invoice schedule, wait for its due time, run maintenance, deliver its published effect through the peer dispatcher, then read the SQL receipt at the destination. |

Run from the repository root with a target directory unique to the checkout on
the mounted Workspace volume:

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-my-worktree \
  cargo test -p crab-cell-app --test reference_application \
  performance::reference_primitive_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

The iteration count is 30 by default and accepts 1 through 1,000. Results print
one `PERF` line per action with sample count, total elapsed time, verified
actions per second, and nearest-rank p50/p95/p99/max action latency. Execution
is serial and includes all method calls, verification, and the 6 ms intentional
Cron due-time wait. It excludes Cell bootstrap and shutdown. Capture the source
revision, Rust profile, CPU, storage, iteration count, and raw output with every
report. These local numbers are development evidence, not a cloud capacity or
release qualification result. Two measured runs are recorded in
[`performance/2026-09-21-local.md`](performance/2026-09-21-local.md).

## Three-runtime fleet workload

The second ignored test places the seven reference Cells across three
independent runtimes. Six primitive lanes run concurrently through signed peer
requests over loopback TCP, with 100 verified actions per lane when the
iteration count is 100. It reports each lane and the fleet's combined action
latency and throughput:

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-my-worktree \
  cargo test -p crab-cell-app --test reference_application \
  performance::reference_three_node_fleet_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

The runtimes share one process and an in-memory object store. Static Cell-ID
routing uses the local TCP stack; this run does not include product ingress,
dynamic placement, mTLS, or provider network latency. The measured topology,
workload, and two runs are in
[`performance/2026-09-21-three-node-local.md`](performance/2026-09-21-three-node-local.md).

## Three-process fleet workload

The process benchmark runs the same six concurrent lanes through three
separate owner processes and a parent load generator. Owners share the
test-only filesystem CAS store and receive signed Cell peer requests over
loopback TCP. Each process has its own node session, SQLite workers, and local
database directory:

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-my-worktree \
  cargo test -p crab-cell-app --test reference_application \
  process_performance::reference_three_process_fleet_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

The test prints action-level and combined fleet latency and throughput. It
starts and stops the three owner processes outside the timed workload. The
topology and two measured runs are recorded in
[`performance/2026-09-21-three-process-local.md`](performance/2026-09-21-three-process-local.md).

## Balanced three-process fleet workload

The balanced variant sends every request through a loopback TCP listener that
selects one of the three owner processes in round-robin order. Any process can
receive a request. It verifies the peer signature, serves a locally owned Cell,
or forwards the unchanged signed operation to the owning process using the
protocol's one allowed forwarding hop. The test asserts that every process
served local requests and forwarded remote requests. It also reports how many
requests the balancer sent to each entry process.

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-my-worktree \
  cargo test -p crab-cell-app --test reference_application \
  process_performance::reference_balanced_three_process_fleet_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

The timer includes the balancer TCP hop and any entry-to-owner forwarding. The
balancer runs as a task in the load-generator process, while three owners run
as separate OS processes. Cell placement is a static seven-Cell test map; this
does not qualify a distributed ownership directory, dynamic placement, or a
million-Cell fleet. Results are recorded in
[`performance/2026-09-21-balanced-three-process-local.md`](performance/2026-09-21-balanced-three-process-local.md).

## Native RustFS sanity check

On 2026-09-22, the ignored typed primitive smoke was also run against a fresh
native RustFS instance and an isolated bucket/prefix. The run reached the
object-store-backed workload but failed the PR profile's measured latency
envelope after 377.94 seconds; RustFS reported roughly five-second commits for
small control objects on the mounted workspace volume. No receipt was emitted
and this is not provider qualification evidence. It is retained here as a
failed environment check so a slow local backend cannot be mistaken for a
passing production profile.
