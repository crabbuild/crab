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

The reference workload creates a fixed 20 repository Cells at the three-node
stage, distributing initialization across those nodes. The same Cells remain
in place as the fleet grows; `--cells` chooses another fixed count. After each scale step it checks the issue
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

The script requires a clean committed checkout. It streams that revision's
Git archive into the existing server Dockerfile and labels the image with its
source revision. It renders a Compose file and node configs, starts three
nodes, and adds 2, 5, and 10 nodes in
successive stages. It fails if a node is unhealthy, its effective Cell memory
is not 1 GiB, a resource limit is missing, a Cell has no live owner, an issue
is not visible through the gateway, or no Cell object reached RustFS. A fresh
project name avoids touching another Compose stack. The default host ports are
`18080` for the gateway, `18101`–`18120` for direct node access, and `19010`
for RustFS on localhost. Choose other ports with `--gateway-port`,
`--node-port-base`, and `--rustfs-port` if needed.

Before startup it checks the image's `org.opencontainers.image.revision` label
against the checkout, including with `--skip-build`. Missing or different
revisions fail. Every server service and the release bootstrap use the inspected
image ID; each node's actual image must match. A project-local
`qualified-<image-id>` tag retains the image when its `local` build tag moves.
Keep that tag while reusing the stack. The load report records the server's
source, platform, and image separately from the load generator's checkout.
Image labels are build metadata; imported images still require the CI artifact's
source and checksum evidence.

To run the gateway workload while each stage has exactly 3, 5, 10, or 20
active nodes, add `--load-stages`. The functional checks remain the default
when this option is omitted. Each rate writes `load-<nodes>-<index>.json`
beside `report.json`; each stage's `loads` array links its ordered points.
The default is five create/read pairs per second for 60 seconds, with at most
64 pairs in flight. `--load-rate` accepts an ordered list, including repeated
controls. Keep it, `--load-duration`, `--load-max-in-flight`, `--load-hot-share`
and `--cells` fixed when comparing node counts. Hot share zero targets Cells
uniformly; a positive fraction directs that portion to Cell 1.

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify.py \
  --state "$state" --project crab-cell-issue-run-1 --load-stages \
  --load-rate 5 20 50 5 --load-duration 60
```

Each point drains publication, checks every acknowledgement and its action
trace, and verifies owner loss before returning. The next point waits for
placement to settle again. Exit 0 means every offered pair was served. Exit 2
retains a partially served point with `integrity_verified: true` and
`passed: false`; the qualifier continues the rate sequence. Broken contracts,
missing acknowledgements/traces, failed drain, uneven successful ingress,
missing resource observations or failed recovery remain fatal.

The top-level report sets `completed: true` only after every point finishes
its checks; `passed` is true only when every point served its offered load.
CI measures 5→20→50→5 pairs/s at each node count, records each point in the job
summary, and continues the separate unpublished-tail fault after a verified
overload. A successful measurement job therefore does not imply all rates
were fully served: inspect `passed` and the raw failures in each receipt.
The final five-pair/s point detects drift, but preceding writes have enlarged
the database; it is not an identical-state causal comparison. These bounded
curves still require longer churn, hot-Cell and independent-host qualification.

### Run a CI-qualified Linux image

The HTTP server container workflow uploads `cell-runtime-image-<run-id>` only
after its image and cluster checks pass. It contains the image archive, SHA-256,
image ID, platform, and exact source revision. On an ARM64 Docker host, dispatch
the workflow on the desired branch with `-f arm64=true`; it uses GitHub's
[native ARM64 runner](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Omit the flag for AMD64. Compare performance only on the same native platform.
Use a run that includes the revision-label check; earlier unlabeled artifacts
are rejected by the current qualifier.

```sh
gh workflow run http-server-container.yml --ref YOUR_BRANCH -f arm64=true
```

To run the rate curve in CI, pass the successful image run to the same
workflow. `fleet_hot_share` forwards the existing `--load-hot-share` option:
use `0` for uniform traffic, `0.8` for skewed traffic, and `1` for one hot Cell.
Run the three shapes separately on the same native platform and image.
Wait for each run to finish before dispatching another on that branch; the
workflow's concurrency group cancels an earlier active dispatch.

```sh
gh workflow run http-server-container.yml --ref YOUR_BRANCH \
  -f arm64=true -f fleet_image_run=YOUR_SUCCESSFUL_RUN_ID -f fleet_hot_share=0.8
```

Each run retains its actual hot share in every load receipt and its CI
summary. The subsequent unpublished-tail fault keeps its separate uniform
five-pair/s workload so it can verify progress on an unaffected owner; a
single-hot-Cell curve does not claim hot-Cell fault qualification.

For a local run, set the successful image qualification run ID below. The
exact-source worktree goes on the mounted workspace volume; the rendered state stays in a directory
Docker can bind-mount. The renderer copies its initialization script into that
state directory, so `--skip-build` does not need a source-tree bind mount.

```sh
run_id=YOUR_SUCCESSFUL_RUN_ID
state="$HOME/.codex/cell-issue-fleet/ci-$run_id"
project="crab-cell-issue-ci-$run_id"
artifact="$HOME/Workspace/crabbuild-target/cell-image-ci-$run_id"
gh run download "$run_id" --name "cell-runtime-image-$run_id" --dir "$artifact"
python3 crates/crab-http-server/deploy/cell-issue-fleet/import_image.py \
  --artifact "$artifact" --project "$project"
image_source="$(cat "$artifact/source-revision")"
git fetch origin "$image_source"
checkout="$HOME/Workspace/Github/crabbuild/crab-cell-ci-$run_id"
git worktree add --detach "$checkout" "$image_source"
python3 "$checkout/crates/crab-http-server/deploy/cell-issue-fleet/qualify.py" \
  --state "$state" --project "$project" --skip-build --load-stages
```

The importer checks the archive SHA-256, hashes its OCI manifest and config,
and matches source/platform receipts before loading. Docker's two archive
entry points must select the same config and layers. The installed image ID
must equal that verified manifest or config digest; a mutable tag is never
identity proof. This handles the observed CI config-ID → Colima manifest-ID
change without accepting unrelated images. Docker's
[classic and containerd stores](https://docs.docker.com/engine/storage/containerd/)
have different image representations, and the
[OCI manifest](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md)
explicitly references its config by digest.

`import-receipt.json` records the archive checksum, both content digests, CI
image ID, installed ID, source, platform, and project tag. Preserve it beside
the downloaded CI artifact. The current importer accepts the workflow's
single-platform OCI export; other archive shapes fail explicitly. It refuses
to overwrite a receipt; use a fresh `--output` for a repeat import.
[Docker load](https://docs.docker.com/reference/cli/docker/image/load/) also
restores the archive's original tags. The importer then creates the selected
project's `local` tag; `qualify.py` pins that image before starting nodes.
The artifact and receipt belong on the mounted workspace volume. Only the
small generated Compose state needs the home-directory bind mount.

To inspect the generated Compose definition without starting Docker:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/render.py \
  --state "$state" --project crab-cell-issue-run-1
docker compose --file "$state/compose.yaml" \
  --profile five --profile ten --profile twenty config --quiet
```

To operate the rendered stack by hand, run these Compose commands in order.
The profiles only add nodes; existing node volumes and RustFS data persist.
These manual builds are exploratory. Use `qualify.py` to produce a pinned stack
for qualification reports:

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

Run this after `qualify.py` has pinned the image and the desired scale stage is
healthy. Pass the number of active
nodes (`3`, `5`, `10`, or `20`) and the number of provisioned Cells. The load
uses one fixed schedule independent of completion time:

```sh
python3 crates/crab-http-server/deploy/cell-issue-fleet/load.py \
  --state "$state" --nodes 20 --cells 20 --rate 5 --duration 60 \
  --max-in-flight 64
```

`--rate` is **create/read pairs per second**, not HTTP requests/s. Each arrival
creates one issue with a stable request ID, then reads that exact issue. Uniform
arrivals rotate through all Cells. `--hot-share 0.8` directs 80% to Cell 1 and
spreads the rest across the remaining Cells; `--hot-share 1` measures a single
hot Cell. Node and Cell counts vary independently. Repeat increasing rates at
a fixed duration and resource envelope; use longer runs to observe repeated
compaction and publication drain.

The scheduler admits at most `--max-in-flight` pairs. It records arrivals it
cannot admit as `client_capacity`; arrivals delayed by a full scheduling
interval are `scheduler_late` and are never issued as a catch-up burst. Both
remain in the offered count. The executor has no unbounded submission queue.
The workload is capped at 100,000 planned pairs and 256 in-flight pairs.
Scheduled pair latency includes dispatch delay and bounded retries. Successful
write/read latency is also reported separately, with sample counts. Failed
requests retain their status/retry reasons; exhausted writes may have an
ambiguous result and keep their original request ID in the raw evidence.

Before load, every Cell must be reachable through every active entry node.
After load, the script waits for every node's uncovered publication bytes to
reach zero, verifies every acknowledged issue and newer RustFS roots, and kills
one owner. After takeover it checks the selected root has not regressed, then
rechecks every acknowledged issue across all Cells before restarting the lost
node. Verification uses at most eight concurrent reads and checks issue
number, title and complete body against each original request's recorded result.
Each scheduled issue body includes its run, Cell and arrival identity, so
substitution from another issue also fails. Creation responses, immediate
readback and functional scale checks use the same comparison. Summary
fields `acknowledgements_before_recovery` and `owner_loss.acknowledgements`
retain verified counts by Cell and verification duration. This checks
published-root recovery; failure during outstanding follower-only tails is a
separate fault gate. Successful ingress counts must be within 70–130% of
an even split. Trace joins identify the actual execution owner and forwarded
write count for every acknowledged write. Read forwarding is not attributed.

Each run writes a summary, sibling `.samples.jsonl` and `.nodes.jsonl` files,
and a `.traces/` directory with node logs and joined `actions.jsonl`. Every admitted,
rejected, late, or failed pair is retained with its arrival index, Cell,
operation timing, and write receipt ID where applicable. A readback mismatch
stops new arrivals after it is observed; already dispatched work is drained.
The summary remains available when post-load verification fails. A missed or
failed arrival, unbalanced ingress, undrained publication, or failed recovery
exits nonzero. Inspect the report to distinguish generator capacity from
service capacity; `passed` is a functional workload result, never a supported
production limit. Schema 7 adds complete-body evidence and a distinct body per
scheduled issue; older runs verified only issue numbers and titles and used a
constant body. Keep the workload version fixed for performance comparisons.
The `source` identifies the load generator; `server`
contains the inspected server image ID, revision label, and platform. Every
running node must match the pinned image before load begins. Retain the CI
image artifact's source proof alongside imported-image reports.

The renderer enables action tracing on each node. Raw samples retain every
HTTP attempt's server request ID, status and latency. After publication drains,
the runner collects logs before owner loss and joins each acknowledged write to
its submission, runtime attempt, Cell/incarnation, owner/session, receipt and
proof. Incomplete or ambiguous joins fail the run. The
[trace runbook](../../REFERENCE.md#attribute-acknowledged-cell-writes) explains
timing overlap, retained evidence, replay commands and current limits. Keep
tracing enabled for comparisons; its overhead is not yet qualified.

The node file records Docker CPU, memory, network/block I/O, and the complete
runtime Prometheus output during load. Collection runs on a separate thread,
with at most four metrics commands at once, 15-second command deadlines, and
a five-second pause between snapshots. Each snapshot records its own time and
collection duration; it is not an atomic fleet view. Observer failures fail the
run. Monitoring itself consumes resources, so keep it enabled for comparisons.
After load, the summary retains per-node uncovered-byte samples until drain or
the 120-second failure deadline.

Verify the scheduler locally with its controllable HTTP service:

```sh
python3 -B -W error::ResourceWarning -m unittest discover \
  -s crates/crab-http-server/deploy/cell-issue-fleet -p 'test_*.py' -v
```

The [gateway load qualification](qualification/2026-09-25-gateway-load.md) and
[stage-load qualification](qualification/2026-09-25-stage-load.md) are historical
closed-loop runs where Cell count equaled node count. Their rates and latency
samples are not directly comparable with this scheduled, fixed-Cell workload.
Sustained offered-rate sweeps, complete action-phase attribution, and multi-host
faults remain necessary for capacity qualification.

### Scheduled harness smoke (2026-09-26)

Local Colima ARM64, real RustFS, 20 Cells, 1-vCPU/1-GiB node limits:

| Check | Result |
| --- | --- |
| Three nodes, 5 pairs/s for 12 s, at most 8 in flight | All 60 pairs passed; 120 successful operations; one 503 retry; publication drained; acknowledged issue survived owner loss |
| Scale the same Cells to five nodes | All nodes healthy; 20 issues and labels retained; owner counts 6/5/5/1/3 |
| Five nodes, 100 pairs/s for 3 s, at most 1 in flight | Expected nonzero exit; all 300 arrival records retained: 14 successful pairs, 283 client-capacity rejections, 3 scheduler misses; publication drained and recovery passed |

The overload case proves generator accounting, not a service throughput limit.
The runner used the working tree based on `c61a3b1d550`; the server image was
`sha256:4ecf6e3e6e83263dbc521fc759c4a877c7f6f5e75fd9c555d25a89ebc7e2555e`,
an earlier local image without source-revision proof. These are harness
functional results, not current-source performance results. The VM supplied
8 CPUs and approximately 16 GiB total. Raw summaries, pair samples, node
snapshots, and owner maps are retained under
`$HOME/.codex/cell-vfs-ltx-scale/arrivals-c61/` as `uniform.*`,
`overload.*`, `startup.json`, and `five-startup.json`.

Healthy ingress does not imply even owner execution. See the
[LTX audit](https://github.com/crabbuild/cellule/blob/70c3c218fafc815b951a40d4df1b50bba15b7d4c/crates/cellule-runtime/docs/ltx-performance-audit.md) for the
execution-distribution and shared-resource gates before scale comparisons.

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
[RustFS action measurements](https://github.com/crabbuild/cellule/blob/70c3c218fafc815b951a40d4df1b50bba15b7d4c/crates/cellule-app/performance/2026-09-25-public-host-rustfs.md).

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
