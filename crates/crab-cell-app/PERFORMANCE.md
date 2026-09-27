# Cell primitive end-to-end performance

## Entity targeting correctness

The entity-ledger gate provisions twelve independent SQL Cells, four on each
of three public `CellNode` hosts. Generated clients use a signed TCP balancer
to prepare commands, resolve durable outcomes, replay duplicates, and read
each Cell at its returned receipt. Reusing one request identity across all
twelve Cells must produce twelve independent records. Stored authority must
confirm 4/4/4 ownership and published roots covering the receipts; each gateway
must execute local and forwarded calls. All node sessions renew and withdraw.

Run against an isolated RustFS bucket with a fresh prefix:

```sh
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab \
CRAB_CELL_TEST_ENDPOINT=http://127.0.0.1:9000 \
CRAB_CELL_TEST_BUCKET=crab-reference-app \
CRAB_CELL_TEST_PREFIX=entity-ledgers-unique-run \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/<checkout>" \
  cargo test -p crab-cell-app --locked --test reference_application \
  entity_ledgers_are_isolated_across_three_rustfs_hosts -- --ignored --nocapture
```

The Compose workflow runs this gate as `driver entities`, retaining
`entities.log`, binary identity, and kernel counters. Its three hosts share
one process and have separate SQLite/cache directories. This is a correctness
check, not a throughput measurement or the many-Cell 3/5/10/20 process profile.

## Scheduled writable entity processes

After preparing the source archive and Linux binary as described under
[three constrained Compose nodes](#three-constrained-compose-nodes), run:

```sh
python3 "$CRAB_REFERENCE_STATE/source/crates/crab-cell-app/qualification/scale.py" \
  --state "$CRAB_REFERENCE_STATE" --project entity-fleet-unique-run \
  --workload entities
```

Every node has four writable entity Cells and a private SQLite/WAL/cache volume.
The fleet grows from 3 to 5, 10, and 20 constrained containers; its workload grows
from 12 to 20, 40, and 80 Cells. Existing Cells retain their owners. This measures
adding owners and workload, not automatic redistribution of a fixed dataset.
Gateways publish the expanded routing table before each stage starts, and the
driver independently checks each stored owner, epoch, incarnation, and target.

Each stage runs three shapes at 1, 4, and 16 scheduled actions per node per second,
with global client concurrency bounded at 4, 16, and 64 respectively. Each point
offers ten seconds of arrivals, then drains admitted calls. A write action
includes its receipt-bound readback; a read action queries the current owner.
Uniform traffic cycles through all Cells. Hot traffic directs 80% of writes to
one Cell and distributes the rest across the other Cells. Skewed traffic sends
80% reads to four Cells and distributes 20% writes across the fleet.

The driver records every scheduled arrival, including late starts, client
saturation, pre-dispatch failures, ambiguous outcomes and resolution, write
receipts, query receipts, and values. Completed samples flush as they arrive so
an interrupted run retains partial evidence. The independent Python verifier
checks exact per-Cell read prefixes and final counts against all acknowledged
write sequences. It reports service and arrival latency, achieved throughput
including drain time, and whether each point served every planned arrival.
An integrity pass does not turn an overloaded point into supported capacity.

`evidence/entity-scaling/control` retains raw TSVs for all 36 points and one-second
node samples: cgroup CPU/throttling and memory, logical local file bytes, active
Cells, admitted SQL/primitive/hydration jobs, retained/disk reservations, gateway
calls, and cumulative object-store operations/bytes. Object counts are logical
backend operations; provider-internal retries are not counted separately.
Per-window resource deltas state their actual sample bounds. Admitted job counts
are not queue-depth measurements. Raw object durability waits are retained per
node. The current host profile uses object durability and owner reads; it does
not measure follower durability or sparse replica-query capacity.

Action durations use `Instant`. Resource samples and windows align using the
shared Linux boot clock from [`/proc/uptime`](https://github.com/torvalds/linux/blob/master/fs/proc/uptime.c),
whose centisecond precision is sufficient for one-second resource samples.
Wall timestamps are retained with their observed adjustment, but do not govern
duration or resource-window validation. Node logs retain runtime warnings,
including SQL deadlines and the error that causes publication, compaction,
renewal, or post-commit execution to fence a Cell.

The Compose workflow runs this profile after reader qualification. Its resource
limits remain 1 CPU/1 GiB per node, while all containers share the recorded Docker
host. Owner-loss recovery, continuous container/schema rollout, workflow and
read-model traffic, actual queue depth, repeated capacity runs, and isolated
multi-host fault domains remain separate qualification gates.

### Qualification status (2026-09-27)

The first Colima/RustFS run used source
`0d4bc19e1c19e63eb226d68e8bed18361d37fef5`. All nine three-node windows
completed, including receipt readback and published-root checks for twelve
Cells. The run then failed during the five-node hot workload at 16 scheduled
actions per node per second: a mutation for entity 13 remained unresolved and
its owner's active Cell count fell from four to three. Ten- and twenty-node
entity stages were not reached. The fencing cause remains unproven.

The same run exposed an independent verifier error: its ten-second duration
check used wall time during a clock correction. A regression now checks the
monotonic window while retaining the wall-clock adjustment. Source
`62e45a44caed99e4ddf9500f4c41a230de9b6f2a` also adds fencing-cause warnings
and retains RustFS file logs. That source passed 23 native reference tests,
16 Python verifier tests, strict app/runtime Clippy and the Linux release build.
Its diagnostic Compose repeat has not been run. These results do not establish
a passing 3/5/10/20 writable-entity profile or supported throughput limits.

## Additive application release correctness

The public-host rollout test compiles a successor SQL module with a new typed
receipt-payload query and retains the predecessor code. An old generated client
uses TCP to invoke the successor host while the successor client writes the
same retained Cell locally. After both writes acknowledge, an operator publishes
the code-only migration. A prepared old capability and a predecessor client
must fail before execution. The upgraded generated client then reads the added
query over signed TCP, replays an old request with its original receipt, and
writes another receipt. A fresh host restores the published root in a separate
SQLite directory, replays the request again without duplication, and writes
successfully. Four visible receipts must remain.

Run the real-provider version against an existing isolated RustFS bucket and a
fresh prefix:

```sh
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab \
CRAB_CELL_TEST_ENDPOINT=http://127.0.0.1:9000 \
CRAB_CELL_TEST_BUCKET=crab-reference-app \
CRAB_CELL_PERF_PROCESS_ROOT=additive-rollout-unique-run \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/<checkout>" \
  cargo test -p crab-cell-app --locked --test reference_application \
  three_node_host_rustfs_additive_code_rollout -- --ignored --nocapture
```

The Compose smoke runs this same gate in a separate constrained driver container
before stopping RustFS, retaining `rollout.log`, its binary hash, and kernel
resource counters. The three initial hosts share that process; one host is
retired before its replacement starts. The schema stays at version one, and
traffic pauses for code publication and recovery. This gate does not establish
rolling-container availability, sustained throughput, or a migration latency
bound. Independent-process scale and fault qualification remain separate.

### Local GA RustFS receipt, 2026-09-27

Source `ea51218b45c7401fac600f2e35aaa75aff30dc35` passed with the digest-pinned
Rust 1.97 and RustFS 1.0.0 images in `qualification/compose.yaml`. A fresh Linux
release build took 3m43s. The binary SHA-256 was
`0dfdedef7b47a28833ba608915149e3f2f7f915b7a1a8227efaf58ef0130fd52`.
Colima supplied four CPUs and 8,307,101,696 bytes of VM memory; each of the
three node containers and both successive driver containers enforced one CPU,
1,073,741,824 bytes of memory and no swap. RustFS shared that VM.

The ordinary three-node smoke passed in 14.93s: 180 complete primitive actions,
one duplicate generated command, twelve replica reads split six per reader,
automatic recruitment/refresh and target-zero eviction. Balancer ingress was
524/524/524; every node performed local and forwarded work. The separately
invoked rollout gate passed in 0.48s with four visible application receipts,
two exact duplicate replays and fresh-host recovery. This short smoke is not
an offered-rate capacity or availability result.

All three node processes withdrew their sessions. Nodes, drivers and bucket
initialization exited zero; RustFS was stopped after evidence capture. Kernel
counters recorded no OOM or CPU throttling. Node memory peaks were
12,853,248–29,618,176 bytes; rollout driver peak was 27,291,648 bytes. Raw logs,
kernel samples, image/container inspections and source/binary identities are
retained under `additive-rollout-ea51218-20260927/evidence` in the external
qualification state directory. Evidence SHA-256 values:

| Artifact | SHA-256 |
| --- | --- |
| `driver.log` | `23bb78bfed1311c8af57e9775af6e743e0f0d35f990058b8c064d5cb2bfd17de` |
| `rollout.log` | `ac481febb24d076295fed3c8582ed35ec917a3de3475ac210ef07481cba5a2b0` |
| `containers.json` | `ed656bffd21f614cc83ffe14f56f3b4b6941a09791eab99c9968beb6d4d1d4ef` |
| `verification.json` | `9ad897b570edca4ea05e3a2573431cd94ee3c8a0af46e35f91f1687ab47eac7c` |

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
newer receipt without a fixture refresh hint; both readers return one additional effect.
Changing the desired-reader policy to zero must evict both views automatically.
Each node proves its retained manager rejects activation and resolution after
host shutdown. Initial admission uses signed owner hints from the host-owned
recruiter; marker files only observe readiness.

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

### Reader recruitment and process loss

The ignored `owner_replaces_killed_reader_through_public_hosts` case uses the
same GA RustFS environment as the process runs. It starts three public hosts,
exercises the reference primitives, admits two readers through signed owner
hints, then starts two more independent processes. It kills one selected
reader, waits for two current readers, and verifies twelve generated queries
against the acknowledged receipt. Writer session, epoch, and incarnation must
remain unchanged. The four survivors must drain successfully.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/<checkout>" \
  CRAB_CELL_PERF_ITERATIONS=5 cargo test -p crab-cell-app --locked \
  --test reference_application owner_replaces_killed_reader_through_public_hosts \
  -- --ignored --nocapture
```

This native fault run does not impose container CPU or memory limits. The
three-node Compose proof below independently verifies the shared recruitment
path under those limits. Neither run establishes sustained capacity or a
recovery SLO.

## Three constrained Compose nodes

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
The [public-host recruitment run](performance/2026-09-27-reader-recruitment.md)
adds automatic signed placement on three constrained nodes and a separate
native three-to-five-process test that replaces a killed reader. The latter
does not impose per-process resource limits.

This is an application integration smoke with six closed-loop lanes and seven
Cells assigned 3/2/2. Ingress counts are even; owner load follows the action
mix. It does not establish a supported throughput, 5/10/20-node application
capacity, mTLS, failure-domain isolation, replica-query throughput, or recovery
during arrivals. The issue-service fleet qualification
covers its separate product ingress and scaling paths.

### Constrained reader scaling and loss

After building the archived source with the procedure above, run the controller
from that same archive with a fresh Compose project:

```bash
python3 "$CRAB_REFERENCE_STATE/source/crates/crab-cell-app/qualification/scale.py" \
  --state "$CRAB_REFERENCE_STATE" --project "$CRAB_REFERENCE_PROJECT-scale"
```

The controller writes a resolved Compose configuration into
`evidence/scaling/`, starts the driver, and follows its bounded scale/fault
requests. Every node and the driver inherit the one CPU / 1 GiB / zero-swap
profile. The driver grows one fleet through 3, 5, 10 and 20 live nodes. The
seven writer Cells remain on the original three owners; new nodes participate
as gateways and admitted readers for the reference SQL Cell.

At each size, generated commands publish two new receipts with duplicate
delivery checks. Both initial recruitment and a later refresh must become
ready automatically. The driver verifies 30 exact queries per selected reader,
records serial read latency, and separately sends owner reads through a TCP
balancer whose entry counts must differ by at most one. Replica requests go
directly to the selected readers through the signed peer transport; this
profile does not place a second balancer in that path.

Each stage also measures sixty seconds of concurrent refresh and queries.
One generated writer schedules five unique mutations per second through the
balancer. It records missed arrivals instead of issuing a catch-up burst.
Eight closed-loop readers run concurrently: four require the latest
acknowledged receipt and four permit older snapshots. Typed `ReplicaBehind`
responses are counted separately; other query errors fail the run. Every
successful snapshot count must match the acknowledged command history at its
actual receipt. All selected readers must serve queries, and the writer must
remain excluded. The final owner read and all selected readers must cover the
last acknowledgement before the next stage.

Raw per-command and per-reader TSV files live in `evidence/scaling/control/`.
The controller independently checks their scheduled arrivals, exact counts,
minimum receipts, lag, and completeness, and binds them by SHA-256 in
`verification.json`. `fully_served_writes` is false when any scheduled write
was missed, even if all admitted work remains correct. Query latency excludes
typed behind responses; their count remains visible. This is a fixed mixed
workload, not a saturation curve or a freshness guarantee.

At five nodes, three readers and one spare are eligible. A separate sixty-second
window keeps the same five scheduled writes/second and eight read lanes active.
Ten seconds into that window, the controller kills a selected reader container
and proves exit 137 without an OOM event. The writer uses the original three
gateways throughout this window; gateway removal and owner failure remain
separate qualifications. Replica reads keep using public selection and its
bounded attempts across the selected readers. Unexpected errors fail the run.

The driver requires an automatically recruited replacement to serve workload
queries before second fifty, leaving at least ten seconds of subsequent load.
After the window, every reader must cover the final acknowledged receipt, and
twelve exact queries check the replacement set. Raw `reader_loss-5-*.tsv` files
and `reader-loss.tsv` retain the workload and fault timeline. The independent
verifier requires successful writes and queries from every read lane wholly
before, during, and after replacement. It reports phase counts and latency,
checks receipt/value correspondence, and preserves missed scheduled writes.
A request spanning the outage cannot count as service during replacement.

The fleet temporarily has four survivors before growing to ten. The killed
boot is never restarted with the fixture's deterministic identity: growth
creates a new node/session, yielding twenty live nodes out of twenty-one
created containers. Writer ownership must remain unchanged. Target zero then
evicts every view, and all surviving hosts must withdraw and drain.

The controller requires successful exact tests, distinct scratch volumes,
matching binary hashes, actual Docker/cgroup limits and no OOM events. It
retains logs, fault events, resource counters and `verification.json`, then
stops only its project. Failed runs retain their containers and evidence.
An optional repeated `--compose-file` supplies explicit image/cache overrides
to the same source configuration; the resolved result is retained.

This combines a scaling/failure smoke and bounded mixed-workload measurements.
One replicated SQL Cell and one killed reader do not establish many-Cell
capacity, gateway-loss handling, arbitrary fault availability or an owner-loss SLO. A one-CPU cap
does not reserve a physical core; record Docker VM resources and contention
before comparing latency across sizes. The Compose CI runs this profile after
the three-node lifecycle smoke using the same compiled binary.
The [2026-09-27 run](performance/2026-09-27-reader-scaling.md) records all four
sizes, 990 measured generated reads, a 34.830-second reader replacement sample,
roughly five-second refresh observations, and successful drain of all twenty
survivors. Query latency and freshness are reported separately.
The [reader-loss-under-load run](performance/2026-09-27-reader-loss-during-load.md)
records two reproduced availability failures, their shared routing and host
recruitment fixes, and a passing raw-data progress check on GA RustFS. All eight
lanes made successful queries during replacement. Its missed writes and host
contention preclude a supported throughput claim.

The [mixed-read/write run](performance/2026-09-27-mixed-readers.md) records
691,427 correct replica reads across four sixty-second windows, with explicit
behind responses and 144 missed scheduled writes. It establishes concurrent
snapshot correctness for that workload, not a supported mixed-load capacity.
The same report records a fresh current-main integration run with 703,981
exact reads, 1,172 acknowledged writes and 28 missed arrivals. Only its
twenty-node window fully served the offered writes; the capacity limit remains
unqualified.

## Native RustFS sanity check

On 2026-09-22, the ignored typed primitive smoke was also run against a fresh
native RustFS instance and an isolated bucket/prefix. The run reached the
object-store-backed workload but failed the PR profile's measured latency
envelope after 377.94 seconds; RustFS reported roughly five-second commits for
small control objects on the mounted workspace volume. No receipt was emitted
and this is not provider qualification evidence. It is retained here as a
failed environment check so a slow local backend cannot be mistaken for a
passing production profile.
