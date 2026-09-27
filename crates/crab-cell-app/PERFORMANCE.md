# Cell primitive end-to-end performance

## Generated client action and recovery slices

The ignored `reference_public_host_action_performance` test runs the generated
reference SQL client on three `CellNode` hosts. Each local or forwarded action
prepares one typed command, waits for its published receipt, and verifies the
row count through a typed query at that receipt. The forwarded lane crosses a
signed peer gateway and then the owner over loopback TCP. The test reports
separate full-action and `execute()`-to-durable-ack distributions. The latter
includes handler execution, transport on the forwarded lane, and publication;
it does not isolate object-store waiting from those costs. The runtime's
`durability_proof` telemetry separately records post-commit submission through
object durability proof, including publication queue time. Recovery timing runs
from owner fencing through authority takeover, exact-root restore, and the
first verified read. It is one recovery sample, so no percentile is reported.

```bash
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-my-worktree \
  cargo test -p crab-cell-app --test reference_application \
  public_host::reference_public_host_action_performance \
  --release --locked -- --ignored --nocapture
```

The local store is in memory, routing is static, requests are serial within
each lane, and one process hosts all three nodes. These results measure the
reference action path and do not establish a supported Cell count, aggregate
throughput, cloud durability latency, or takeover SLO.
The two local release runs are recorded in
[`performance/2026-09-25-public-host-action.md`](performance/2026-09-25-public-host-action.md).

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

## Three-process RustFS workload

The process benchmark runs the six concurrent action lanes through three
public `CellNode` hosts. Each process owns its SQLite workers, WAL/cache
files, signed renewable node session, and ordered shutdown. RustFS stores
Cell authority, LTX objects, and session records. The parent binds public
application handles; generated stable-ID commands additionally prove that
repeating a committed mutation preserves its receipt and creates one visible
effect. Every primitive lane checks its visible result.

After the measured action lanes, the generated-client proof admits two SQL
snapshot readers on the other node processes through the host-owned
`ReadReplicaManager`. It verifies missing readers do not fall back to the owner,
then issues a new owner mutation twice. The supervisor must discover its exact
newer receipt without a refresh hint; both readers return one additional effect.
Changing the desired-reader policy to zero must evict both views automatically.
Each node proves its retained manager rejects activation and resolution after
host shutdown. Initial admission is triggered by fixture markers; recruitment
after node loss remains separate product qualification.

Successful direct peer replies are counted by selected physical node: six
per reader, zero on the writer. Each receiver executes the explicit query
against its local admitted snapshot, including gateway receivers. The fixture
observes each reader's receipt before testing the new position, so background
refresh timing cannot make a stale-position assertion flaky. Deterministic
stale/minimum-receipt checks remain in the runtime suite and the earlier
source-bound Compose report. These steps are outside the action timer and do
not establish replica throughput.

Provide an isolated bucket, a prefix, and explicit credentials:

```bash
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab \
CRAB_CELL_TEST_ENDPOINT=http://127.0.0.1:9000 \
CRAB_CELL_TEST_BUCKET=crab-reference-app \
CRAB_CELL_TEST_PREFIX=reference-performance \
CRAB_CELL_PERF_ITERATIONS=30 \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-my-worktree \
  cargo test -p crab-cell-app --test reference_application \
  process_performance::reference_balanced_three_process_fleet_end_to_end_performance \
  --release --locked -- --ignored --nocapture
```

Each invocation adds a unique object prefix. The balanced variant sends every
request to a round-robin TCP gateway. Each receiving node verifies the peer
signature, dispatches locally or forwards once to the owner. The test requires
local and forwarded results on every node. The direct variant,
`reference_three_process_fleet_end_to_end_performance`, omits the gateway.
Both measure complete actions; setup and shutdown remain outside the action
timer. Node object-proof wait samples include the whole process lifetime.

The historical September 21 filesystem measurements remain in
[`performance/2026-09-21-three-process-local.md`](performance/2026-09-21-three-process-local.md)
and [`performance/2026-09-21-balanced-three-process-local.md`](performance/2026-09-21-balanced-three-process-local.md).
They do not describe the current RustFS path.

### Three constrained Compose nodes

[`qualification/compose.yaml`](qualification/compose.yaml) pins GA RustFS 1.0.0
and the build image by digest. It runs three independent nodes plus a driver
containing the TCP balancer. Each node is limited to one CPU, 1 GiB memory,
zero swap, and 256 processes. Each has a private disk-backed Docker volume for
SQLite/WAL/cache; the shared evidence directory holds control markers and
reports. The `crab`/`crab` credentials are local fixture credentials. No service
publishes a host port.
The disposable evidence directory is shared and writable by the host and all
fixture containers; its sticky bit protects entries owned by another UID.
This also permits capability-free container root to create logs on a Linux
runner-owned bind mount. Source and binary mounts remain read-only.

The worker pool retains its 32-writer ceiling and explicitly reserves 32 MiB
for native admission, covering a reader's overlapping refresh snapshots.
The default 32-writer budget alone is 2 MiB and correctly refuses a 12 MiB
snapshot; raising the writer count is not required to provision reader memory.

Use a fresh Compose project and state directory per run. The source archive
must contain the committed change being measured. The selected Docker/Colima
VM must mount the external state directory and have space for the build:

```bash
export CRAB_REFERENCE_STATE="$HOME/Workspace/crabbuild-target/crab-my-worktree/reference-$(git rev-parse --short HEAD)-$(date +%s)"
export CRAB_REFERENCE_PROJECT="crab-reference-$(date +%s)"
mkdir -p "$CRAB_REFERENCE_STATE/source" "$CRAB_REFERENCE_STATE/target-linux" "$CRAB_REFERENCE_STATE/evidence"
chmod 1777 "$CRAB_REFERENCE_STATE/evidence"
git archive HEAD | tar -x -C "$CRAB_REFERENCE_STATE/source"
git rev-parse HEAD > "$CRAB_REFERENCE_STATE/evidence/source-revision.txt"
compose() {
  docker compose -p "$CRAB_REFERENCE_PROJECT" \
    -f "$CRAB_REFERENCE_STATE/source/crates/crab-cell-app/qualification/compose.yaml" "$@"
}
compose run --rm build
compose up -d node-0 node-1 node-2
compose run --name "$CRAB_REFERENCE_PROJECT-driver" driver
compose ps -a
compose logs --no-color > "$CRAB_REFERENCE_STATE/evidence/compose.log"
docker inspect $(compose ps -aq) > "$CRAB_REFERENCE_STATE/evidence/containers.json"
```

The wrapper verifies the actual cgroup CPU/memory/swap limits and exactly one
executed test. It retains the binary hash, per-node kernel CPU and peak-memory
counters, active Cell and retained-byte observations, object-proof wait
samples, action timings, duplicate-delivery proof, and gateway counts. Require
all three nodes and the driver to exit zero. A driver failure leaves nodes
available for diagnosis; use `compose stop` after collecting evidence. Remove
only this project's containers/volumes with `compose down -v` when their
artifacts are no longer needed.

The [initial 2026-09-27 run](performance/2026-09-27-three-node-compose.md)
records the owner action path. The
[generated replica-read follow-up](performance/2026-09-27-replica-compose.md)
records two non-owner readers, stale/minimum-receipt checks, controlled refresh,
source and binary identity, and container resource evidence on GA RustFS.
The [host-owned reader run](performance/2026-09-27-host-readers-compose.md)
then proves automatic refresh, target-zero eviction, and terminal drain through
the manager shared with the product server.

This is an application integration smoke with six closed-loop lanes and seven
Cells assigned 3/2/2. Ingress counts are even; owner load follows the action
mix. It does not establish a supported throughput, 5/10/20-node application
capacity, automatic reader placement/replacement, mTLS, failure-domain
isolation, replica-query throughput, or recovery during arrivals. The issue-service fleet qualification
covers its separate product ingress and scaling paths.

## Native RustFS sanity check

On 2026-09-22, the ignored typed primitive smoke was also run against a fresh
native RustFS instance and an isolated bucket/prefix. The run reached the
object-store-backed workload but failed the PR profile's measured latency
envelope after 377.94 seconds; RustFS reported roughly five-second commits for
small control objects on the mounted workspace volume. No receipt was emitted
and this is not provider qualification evidence. It is retained here as a
failed environment check so a slow local backend cannot be mistaken for a
passing production profile.
