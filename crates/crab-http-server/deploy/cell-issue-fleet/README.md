# Cell issue fleet example

This disposable Docker Compose stack runs Crab's existing issue and label
service as a Cell application. Each repository owns a durable SQLite Cell.
Requests enter any node, route to the current owner over the signed peer
protocol, and publish the Cell root to RustFS. New node containers join the
same fleet at 3, 5, 10, and 20 processes.

```mermaid
flowchart LR
    Client[Issue client] --> Gateway[Caddy gateway]
    Gateway --> Nodes[3 → 5 → 10 → 20 Crab nodes]
    Nodes -->|signed owner routing| Nodes
    Nodes -->|Cell controls and LTX roots| RustFS[(RustFS)]
    Nodes --> Local[Per-node local Cell volume]
```

The reference workload creates one repository Cell per node, then writes an
issue and label through that node. After each scale step it checks the issue
through the gateway, verifies every Cell has a live owner and durable root,
and reads the original Cell through every newly added node. It also kills the
owner of the last Cell, verifies a new owner serves the acknowledged issue
without regressing the published root, and restarts the lost node. The resulting
`report.json` records ownership spread and each node's live admission
envelope. This is a functional scale-up and routing check, not a throughput
or capacity claim.

The gateway uses round-robin routing across healthy nodes. The separate load
qualification below sends equal issue writes and reads to every Cell through
that gateway and records the entry node on each response.

Each node container has a Docker limit of **1 vCPU and 1 GiB memory**, no swap,
and its own persistent local Cell volume. The runtime applies a 30 GiB logical
local disk admission limit per node and still checks actual free space. Docker
named volumes do not reserve 30 GiB apiece, so the host needs enough shared
disk for the workload. A network-namespace keeper lets all Crab listeners stay
on loopback, as required by this unauthenticated local example. Containers have
separate processes, cgroups, and local volumes; this does not simulate separate
pod network namespaces or a multi-host failure domain.

RustFS is reachable from the Compose network and from host loopback only. Its
**disposable local** access key and secret key are both `crab`; do not expose
this stack to other machines.
The RustFS volume and the peer identity volume persist across the four stages.
The script never removes them automatically.

## Run

From the repository root, choose a fresh project name and a state directory
outside the checkout. Docker must be able to bind-mount that directory. The
small rendered configs can live under the home directory; Docker Desktop and
Colima mount it by default:

```sh
state="$HOME/.codex/cell-issue-fleet/run-1"
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify.py \
  --state "$state" --project crab-cell-issue-run-1
```

The script builds the existing `crab-http-server` image, renders a Compose
file and node configs, starts three nodes, and adds 2, 5, and 10 nodes in
successive stages. It fails if a node is unhealthy, its effective Cell memory
is not 1 GiB, a resource limit is missing, a Cell has no live owner, an issue
is not visible through the gateway, or no Cell object reached RustFS. A fresh
project name avoids touching another Compose stack. The default host ports are
`18080` for the gateway, `18101`–`18120` for direct node access, and `19010`
for RustFS on localhost. Choose other ports with `--gateway-port`,
`--node-port-base`, and `--rustfs-port` if needed.

To inspect the generated Compose definition without starting Docker:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/render.py \
  --state "$state" --project crab-cell-issue-run-1
docker compose --file "$state/compose.yaml" \
  --profile five --profile ten --profile twenty config --quiet
```

To operate the rendered stack by hand, run these Compose commands in order.
The profiles only add nodes; existing node volumes and RustFS data persist:

```sh
docker compose --file "$state/compose.yaml" build release-init
docker compose --file "$state/compose.yaml" up --detach --no-build --wait
docker compose --file "$state/compose.yaml" --profile five \
  up --detach --no-build --wait
docker compose --file "$state/compose.yaml" --profile five --profile ten \
  up --detach --no-build --wait
docker compose --file "$state/compose.yaml" \
  --profile five --profile ten --profile twenty \
  up --detach --no-build --wait
```

The checked-in RustFS, AWS CLI, Caddy, Node, Rust, and Debian images are pinned
by the underlying Compose generator or Dockerfile. The gateway serves
<http://127.0.0.1:18080/api/repos/demo/work-01/issues?state=all> after stage
three. To inspect the current containers:

```sh
docker compose --file "$state/compose.yaml" \
  --profile five --profile ten --profile twenty ps
cat "$state/report.json"
```

## Qualify gateway distribution and Cell actions

Run this after the desired scale stage is healthy. Pass the number of active
nodes: `3`, `5`, `10`, or `20`. The example below exercises the full 20-node
fleet with one concurrent client lane per Cell and 10 create/read pairs per
lane:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/load.py \
  --state "$state" --nodes 20 --pairs-per-cell 10
```

The script first reads every Cell through every active entry node. It then
sends the same number of writes and reads to each Cell, verifies each write's
readback, checks that successful entry traffic is within 70–130% of an even
split, and waits for a newer RustFS root for every Cell. Finally it kills one
owner, reads its last acknowledged issue through the gateway after takeover,
checks that the root did not regress, and restarts the killed node. Temporary
busy or unavailable responses are retried a bounded number of times; writes
reuse their original request ID. The JSON report includes retry counts and
end-to-end latency, including retry waits. A failed run exits nonzero.

Each run writes `load-<nodes>-<run-id>.json` under the state directory. The
measurement is the throughput of this fixed client workload on one machine,
not maximum fleet throughput or a production SLO. See the
[gateway load qualification](qualification/2026-09-25-gateway-load.md) for
one local run and its limits.

## Measure public-host actions against RustFS

With the stack running, this ignored release-profile test runs 100 serial
verified local actions and 100 serial forwarded actions through the reference
application's public host API. Its Cell roots, LTX objects, and recovery reads
use the Compose RustFS bucket. The result reports action throughput, latency,
object durability wait, and one owner-loss recovery observation. Set the
endpoint port to the value passed to `--rustfs-port`. Use a Cargo target
directory unique to the checkout; the example path below is for the main
checkout:

```sh
CRAB_CELL_TEST_BUCKET=crab-cell-issue-fleet \
CRAB_CELL_TEST_ENDPOINT=http://127.0.0.1:19010 \
CRAB_CELL_TEST_PREFIX=reference-performance \
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab \
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo test -p crab-cell-app --test reference_application \
  public_host::reference_public_host_rustfs_action_performance \
  --release --locked -- --ignored --nocapture
```

Each run gets a distinct object prefix. The application hosts in this test are
still three processes on one machine; they use the Compose RustFS service but
do not use the 20 Compose node processes. Keep the raw test output to compare
RustFS measurements with the in-memory baseline. Serial actions do not
establish saturation throughput or a production SLO. See the
[RustFS action measurements](../../../crab-cell-app/performance/2026-09-25-public-host-rustfs.md).

To stop this **disposable** project while preserving its data, use `down`
without `--volumes`. Removing its volumes deletes the RustFS data, peer
identity, and every node's local Cell volume.

The RustFS service has an explicit 65,536 descriptor limit; the qualification
runner verifies it and records provider descriptor counts. The earlier 1,024
soft limit exhausted during post-churn membership scans and produced S3 500
errors. This fixture limit is independent of Crab node resource admission.

The 1 GiB profile is an evaluation profile. This single-machine Compose run
cannot establish a supported production Cell count, recovery SLO, cloud-store
durability, independent network failure behavior, or multi-host throughput.

## S3-rooted read replicas

Use a fresh project and state directory for the object durability profile:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify_read_replicas.py \
  --state "$HOME/.codex/cell-issue-fleet/read-replicas-1" \
  --project crab-cell-issue-read-replicas-1
```

This run applies read-replica targets of 2, 4, 9, and 19 as the fleet grows
from 3 to 20 nodes. It queries the original issue through an explicit replica
route and records each serving node from `x-crab-cell-reader` in
`read-replica-report.json`. Every Cell mutation uses the object durability
profile. Each stage compares 200 owner and 200 replica reads at concurrency
eight, reports p50/p99 latency and actual reader distribution, and samples
process memory, descriptors, local disk, and runtime metrics. Before/after
per-node counter snapshots record control-record loads, LTX fetches and bytes,
and logical page reads for both workloads. Raw series and collection windows
are retained; amortized costs include background work and collection skew.
They exclude membership/policy reads and retries hidden inside the provider,
so they are not total S3 billing-request counts.

Each stage then acknowledges an issue-body update and polls every selected
reader until its receipt covers the inspected authority root and its body is
correct. The report records sequence lag, unavailable attempts, each reader's
first observed fresh response, and per-node LTX bytes during that window.
Freshness times are polling upper bounds, including authority inspection;
refresh bytes include background and query-fault traffic on reader nodes.
Missing metrics, counter resets, changed nodes, or a stale value at a covering
receipt fail qualification. Parser/evidence checks run with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s crates/crab-http-server/deploy/cell-issue-fleet -p 'test_*.py'
```

These samples are not peak-resource or production-capacity measurements. A reader-only
failure must recruit a replacement without changing the writer or its epoch.
A separate primary-only failure must select one of the two verified warm readers and
successfully publish a new comment afterward.
The target-count phase exercises 0→1→2→4→1, verifies zero-target rejection,
and kills a selected reader before shrinking from four to one. Each step
checks actual selected/ready counts and unchanged writer authority.

At 20 nodes the runner also kills a Cell's owner and two observed
readers, removes those three disposable local Cell volumes, and requires a
survivor to recover the acknowledged issue and recruit two new readers from
RustFS. The recovered writer must then acknowledge a new issue-body update,
serve it, and advance the S3 root under the same owner and epoch.
Finally it pauses RustFS, requires explicit replica reads to return
`replica_unavailable` without data, resumes the provider, and verifies recovery
without a receipt regression. Each fault phase first establishes a serving
Cell because an earlier killed reader can own other Cells. Node inspection
selects the unique live advertised boot session, including after restarts.
The report proves local RustFS side effects and observed reader distribution
on one host; it does not replace protected-provider evidence.

An additional fault runner uses the existing twenty-node project's image:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify_reader_partition.py \
  --state "$HOME/.codex/cell-issue-fleet/read-replicas-1"
```

It routes one non-owner node's S3 endpoint through a disposable Caddy proxy,
proves that node serves a replica, then pauses only that proxy. HTTP and peer
networking remain available. It requires the isolated ingress to fail closed,
kills the owner, and requires a healthy successor to advance the epoch and
acknowledge a new S3-rooted mutation. The isolated session cannot be that
successor. After its lease expires the server closes its listener; an empty
gateway 502 is accepted only with a fresh expired-session record and no OOM kill.
Cleanup resumes the proxy, restores both nodes and the original
endpoint, and leaves the proxy stopped. The separate receipt records runtime,
image, runner, source-report hash, and control states. This is an S3-path
partition on one host, not independent-network or multi-host qualification.
The proxy preserves signed request headers and uses explicit HTTP with
compression disabled ([Caddy contract](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy#defaults)).

The completed [local qualification receipt](qualification/2026-09-25-read-replicas.md)
records exact runtime/runner/image identities, distribution, latency, resources,
and fault results.

## Fleet-to-object rollout qualification

Use a fresh disposable project for the three-node mode transition:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify_mode_rollout.py \
  --state "$HOME/.codex/cell-issue-fleet/mode-rollout-1" \
  --project crab-cell-issue-mode-rollout-1
```

The runner first drives bounded concurrent writes and requires enrolled logs
and observed fleet durability proofs, then writes a comment and stops all
three servers. Fleet mode may also complete writes through object proof;
individual idle logs need not have won a follower-proof race. It changes their
configs only after every server exits successfully without an OOM kill.
Successful shutdown includes the existing node-log coverage barrier: every
issued frame must be object-covered before the durable log close CAS.
A failed or forced drain leaves the fleet configuration intact and fails the
run. The provider and all local volumes remain available for investigation.

Add `--exercise-drain-faults` to temporarily give RustFS 0.25 vCPU during
fleet enrollment, restoring its normal CPU schedule before fault injection.
This makes follower proofs observable against a slower object provider. The
runner then kills both members of an observed active
durability log, then separately stop RustFS before drain. Each interrupted
attempt must fail the same rollout barrier with byte-identical fleet configs;
restart must recover every acknowledged value before the next phase. The
runner then completes the normal mode transition and all-disk-loss checks.
Using a prebuilt image requires `--skip-build --runtime-source <commit>` so
the report distinguishes runtime source from qualifier source.

After restarting in object mode, the runner verifies old values, publishes a
new comment, checks object proof counters with no new fleet proofs, and
activates two readers. It then kills all three servers, deletes only their
project-labeled Cell volumes, restarts fresh nodes, and requires both the
pre-rollout and post-rollout comments, including every acknowledgement from
the fleet-proof workload, plus the original issues and labels to survive. The result is saved in `mode-rollout-report.json`.
This is an offline rollout for the local fixture; platform rollout and
protected-provider release procedures remain separate.

## Reader drain and offline retention

An existing disposable twenty-node reader fixture can qualify retention after
its unreachable objects have aged beyond the CLI's one-hour minimum grace.
Stop all its application nodes first. Build the image from a clean commit and
record that commit separately from the runner revision:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify_reader_retention.py \
  --state "$HOME/.codex/cell-issue-fleet/read-replicas-1" \
  --image crab-cell-issue-read-replicas-1:local \
  --runtime-source <image-source-commit>
```

The runner resumes three nodes, verifies twenty issues and two serving readers,
creates a backup pin, and enters maintenance through the public CLI. A one-object
deletion budget must leave the release in Maintenance and every serving process
cleanly drained. Retrying the same prepared revision completes the sweep. Raw
provider inventories must match the deletion counters and the grace cutoff;
the retained pin must still verify. Restarted nodes must recover the same issue
and comment data, recruit two readers, and acknowledge a new write. The runner
retains logs and its incremental `reader-retention-report.json` on failure.
If interrupted after the incomplete one-object pass, repeat the command with
`--resume`; it verifies the fixture and exact maintenance authority, records
the resumed runner and image source, and retries the same prepared revision.
An updated executor image must still match the prepared application descriptor.
It never lowers the grace period or rewrites object timestamps. Use only the
disposable fixture: this command actually deletes eligible immutable objects.

After stopping that fixture, qualify loss of every reader before the writer:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify_reader_first_loss.py \
  --state "$HOME/.codex/cell-issue-fleet/read-replicas-1"
```

This resumes three nodes using the successful retention receipt's image. It
proves two readers, kills them and deletes their project-owned Cell volumes,
then requires a new object-backed acknowledgement from the unchanged writer.
Only afterward does it kill the writer and delete its Cell volume. Fresh
nodes must recover the acknowledged value at a new epoch and recruit two
readers at or above that mutation's durable sequence. The provider and its
volumes remain intact; `reader-first-loss-report.json` records both ownership
cuts, the deleted local volumes, source/image identities, and recovery timing.

`qualify_reader_load.py --state <same-state>` then resumes five nodes for a
60-second owner workload and a 60-second replica workload. Eight closed-loop
clients assign three requests to node 1 followed by one to node 5, repeating
that schedule regardless of which ingress completes faster. The total
concurrency stays within the eight-request admission limit at each ingress.
The report records actual reader receipts/counts per ingress, throughput,
latencies, control/LTX counter deltas, and process resources. `VmHWM` is the
process-lifetime resident high-water mark; disk and descriptor counts are
boundary samples. Any HTTP error, wrong value, old receipt, or missing reader
fails the run while retaining evidence. This is a bounded single-host load
measurement, not a production capacity or soak-test claim.

`qualify_reader_perf.py` reuses that retained fixture to compare optimized
images with the owner route. Pass `--state <same-state> --report <new-path>
--image <image> --runtime-source <commit>`; new images must carry the matching
`org.opencontainers.image.revision` label. It verifies running image IDs and
node limits, uses the same eight clients and sixty-second windows, and runs
three pairs in alternating order. `--rounds 1` provides an initial experiment.
Each run cleanly drains this disposable fixture, starts node 5 alone to claim
the Cell, then starts the other nodes. This holds physical ownership constant
between images: node 1 routes to a remote primary and node 5 serves it locally.
The report records ownership before and after each pair and rejects a changed
owner, epoch, or incarnation. Per-ingress latency and throughput expose local
owner traffic separately. Reports also verify the fixed 3:1 request mix. Earlier
reports using six clients pinned to node 1 and two to node 5 used a different
driver: their completed request mix varied by mode, so aggregate percentiles
from that driver cannot establish parity under identical ingress traffic.
Every pair must have zero errors, correct values/receipts, all four readers,
replica throughput at least 80% of owner throughput, and replica median/p99
latency at most 120% of owner latency. Reports and derived Compose files are
retained on failure; these limits apply only to the recorded local workload.
After passing every pair, the runner pauses RustFS and requires a replica
request to fail closed with HTTP 503. It then resumes RustFS and checks the
same incarnation, a non-regressing receipt, and the acknowledged issue value.
