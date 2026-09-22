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
release qualification result.
