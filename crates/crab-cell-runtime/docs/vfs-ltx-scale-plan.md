# Execute the SQLite VFS and LTX Cell scaling plan

Crab already runs SQLite locally over a verified sparse VFS and captures LTX
from its WAL. This plan hardens the request path around that implementation,
qualifies cold recovery and durability, and proves an application made of many
Cells. It does not introduce a shared writable SQLite file or another
replication protocol.

| Document intent | Value |
| --- | --- |
| Content type | Technical design and executable delivery plan |
| Audience | Runtime, HTTP, application, and qualification contributors |
| Status | Proposed work; the baseline commands below run against the current tree |
| Decision | Fix owner routing first; change pager or durability policy only after phase-specific evidence |

[Back to the Cell runtime index](README.md)

## Use the existing implementation

| Responsibility | Existing owner | Contract to preserve |
| --- | --- | --- |
| Product authentication, authorization, and ingress | [HTTP router](../../crab-http-server/src/cells/router.rs) and [peer receiver](../../crab-http-server/src/peer.rs) | Authorize the target and action before dispatch; bound peer hops |
| Cell identity, owner, epoch, lifecycle, root | [Cell authority](../src/control/authority.rs) | Only conditional control writes grant or change ownership |
| Actor admission and one SQL writer | [Cell actor](../src/cell/actor.rs) | A fenced or draining actor refuses queued and new work |
| Local SQLite, WAL capture, LTX | [Db](../../crab-ltx/src/db.rs) | Local commit alone never releases a response |
| Sparse exact-root page access | [Writable VFS](../../crab-ltx/src/writable_vfs.rs) and [paged I/O](../../crab-ltx/src/paged_io.rs) | Verify inherited pages, reserve disk, and create a fresh local file |
| Durable acknowledgement | [Follower design](failover-and-followers.md) and [publication](../src/publication.rs) | A response follows object-root proof or the selected followers' fsync proof |
| Application-level cross-Cell work | [Application framework](application-framework.md) | One Cell transaction; durable effects and idempotent inboxes across Cells |

The request and recovery paths should remain:

```text
client -> gateway -> entry node -> owner hint or authoritative slow path
       -> authenticated peer hop if needed -> actor admission -> local SQLite
       -> LTX -> follower fsync proof OR immutable root + authority CAS
       -> response

owner loss -> seal/recover acknowledged follower tail -> exact root
           -> fresh sparse SQLite file -> verified page faults -> resident Cell
```

A route hint, placement plan, or cached page is never Cell authority. A
follower log stores recent LTX; it does not serve SQL. A durable response may
precede object publication only when the selected follower proof covers it.
The [canonical scaling contract](canonical-ltx-scaling.md) excludes read
replicas and hot SQL standbys. Revisit that decision only after measured owner
read saturation.

## Baseline the current tree

The [20-node gateway record](../../crab-http-server/deploy/cell-issue-fleet/qualification/2026-09-25-gateway-load.md)
is the comparison baseline, not a supported limit. In its fixed-load phase,
385 of 400 requests entered nonowners, 23 HTTP 503 attempts were retried,
read p95 was 389.089 ms, and write p95 was 783.251 ms. The observed 56.22
logical requests/s is not saturation throughput. The three-node public-host
[RustFS action record](../../crab-cell-app/performance/2026-09-25-public-host-rustfs.md)
isolates local and forwarded actions but does not exercise the 20 Compose
nodes. Both runs used one machine.

Run from the repository root. Choose a fresh project and state directory for
each comparison run. This disposable stack uses local RustFS credentials
`crab/crab`; never expose it outside the local Docker network. The
`qualify.py` command builds the image and performs the 3 -> 5 -> 10 -> 20
functional scale check. The existing `load.py` command then measures the
20-node stage. Keep all raw JSON and logs outside the checkout.

```sh
state="$HOME/.codex/cell-vfs-ltx-scale/$(date +%Y%m%d-%H%M%S)"
project="crab-cell-issue-vfs-ltx-$(date +%s)"
python3 crates/crab-http-server/deploy/cell-issue-fleet/qualify.py \
  --state "$state" --project "$project"
python3 crates/crab-http-server/deploy/cell-issue-fleet/load.py \
  --state "$state" --nodes 20 --pairs-per-cell 10
```

The first command writes `report.json`; the second writes a
`load-20-*.json` report. Do not read `--nodes 3`, `5`, or `10` from a
stack that has already reached 20 nodes as a stage result. Work packet 1
extends the runner to load each stage while that stage is actually active.
For a fast syntax-only check before building images:

```sh
state="$HOME/.codex/cell-vfs-ltx-scale/render-check"
python3 crates/crab-http-server/deploy/cell-issue-fleet/render.py \
  --state "$state" --project crab-cell-issue-vfs-ltx-render
docker compose --file "$state/compose.yaml" \
  --profile five --profile ten --profile twenty config --quiet
```

For local Rust tests, first verify `$HOME/Workspace` is mounted and writable.
Use a Cargo target directory dedicated to this worktree, for example
`$HOME/Workspace/crabbuild-target/crab-8bc8`; set
`CARGO_TARGET_DIR` on every Cargo invocation. The existing RustFS action
test requires a running Compose stack and its bucket:

```sh
CRAB_CELL_TEST_BUCKET=crab-cell-issue-fleet \
CRAB_CELL_TEST_ENDPOINT=http://127.0.0.1:19010 \
CRAB_CELL_TEST_PREFIX=reference-performance \
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab \
CRAB_CELL_PERF_ITERATIONS=100 \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-cell-app --test reference_application \
  public_host::reference_public_host_rustfs_action_performance \
  --release --locked -- --ignored --nocapture
```

This action test runs three application hosts in the test process against
RustFS; it is a different topology from the Compose fleet. Its result must
be labeled separately in any comparison.

## Work packet 1: make the qualification comparable

**Change:** extend the existing Compose qualifier to run the current
`load.py` workload at 3, 5, 10, and 20 nodes immediately after each
stage becomes healthy. Keep the existing command as the default functional
check. Save one raw load report per stage with source revision, image digest,
Compose profile, node limits, RustFS identity, workload size, and retry counts.
The runner must not silently retry a mutation with a new request ID.

**Files:** [qualify.py](../../crab-http-server/deploy/cell-issue-fleet/qualify.py),
[load.py](../../crab-http-server/deploy/cell-issue-fleet/load.py), and the
[Compose example guide](../../crab-http-server/deploy/cell-issue-fleet/README.md).

**Proof:** each stage uses exactly its active nodes and Cells; the gateway
entry histogram remains within the existing 70-130% even-split check; every
write is read back; each Cell advances its RustFS root; the owner-loss case
recovers the last acknowledged issue. Record p50/p95/p99 and max for reads,
writes, and recovery, plus retry and error counts. Do not set a production
latency or Cell-count limit from one shared-host Compose run.

**Exit:** four stage reports are retained and can be compared to a second
run with the same profile. A report with retries is valid evidence but is
not an error-free service result.

## Work packet 2: remove redundant peer resolution

The entry [router](../../crab-http-server/src/cells/router.rs) checks for an
actor-owned resident handle, then on a miss reads catalog, exact control,
and live-owner state. On the receiving node,
[peer handling](../../crab-http-server/src/peer.rs) resolves the target to
decide whether to activate or forward, then
[dispatch](../src/peer/dispatch.rs) resolves it again before execution.
This duplicates object-store metadata work on the common forwarded path.

**Change:** make the receiver's one resolution available to dispatch through
a private, request-scoped result. The result may carry only a local handle
that still passes actor admission, or a typed not-local outcome. Preserve
peer signature verification, product reauthorization, tenant/application
target checks, remaining deadline, activation policy, and the existing hop
limit. If migration races with dispatch, the actor must refuse or fence the
request; the receiver must not execute using stale authority.

**Proof:** an instrumented store observes one receiver resolution rather
than two on a forwarded read; owner-local resident reads still make zero
object-store calls; malformed/unauthorized/stale peer requests fail closed;
takeover, drain, and compatible rollout tests retain their existing outcomes.
Use the current [public takeover test](../../crab-http-server/tests/public_cell_takeover.rs)
and [runtime residency tests](../tests/runtime/lifecycle/residency.rs)
as entry points, then add one targeted receiver test for metadata calls and
fencing. Do not add a second dispatch path for compatibility.

**Exit:** identical results under owner loss and at least one fewer catalog
head, catalog page, and control read per healthy forwarded request.

## Work packet 3: give ingress a bounded owner hint

After packet 2, measure whether entry-node metadata remains the dominant
forwarded cost. If so, keep one short-lived, process-local owner observation
per Cell target in `crab-http-server`. Populate it only from an authenticated
successful slow-path resolution and a live signed node advertisement. Use it
only to choose the peer destination. The receiver still authorizes and admits
the request; the Cell authority CAS remains decisive. A hint must never
create, transfer, or renew ownership.

Set a fixed maximum lifetime no longer than the observed node-session lease;
do not add a public config option. Invalidate on peer refusal, stale session,
target mismatch, release mismatch, and node-liveness loss. Retry the
authoritative slow path at most once within the original deadline. Keep
normal routing to one peer hop and the existing bounded stale-owner redirect
behavior. Cache keys and telemetry labels must not contain arbitrary Cell
IDs or tenant strings.

**Proof:** a warm, healthy forwarded read performs zero entry-node
catalog/control object reads, while a stale hint routes through the slow
path or fails closed without executing on a fenced owner. Test deletion,
drain, owner loss, rollout, simultaneous hint expiry, and a peer that
returns an ambiguous transport result. Report cache hit/miss/stale counters
and end-to-end latency; never infer correctness from a cache hit.

**Exit:** under the same 20-node workload, read and write p95 improve
against packet 1 without raising retries, 503s, or object-store requests
per logical action. If the measured benefit is absent, remove the hint
and keep packet 2's simpler routing path.

## Work packet 4: qualify the existing pager and durability paths

Do not replace the [writable VFS](../../crab-ltx/src/writable_vfs.rs).
Exercise cold open, page fault, background hydration, resident promotion,
idle eviction, and takeover over RustFS. A verified resident read has zero
object-store calls; sparse reads count their exact root/page requests.
Record p50/p95/p99, bytes, page I/O queue depth, disk reservations, and
time to first read. Reject corrupted page digests, a changed exact root,
insufficient disk, and a canceled hydration without reusing an unverified
mutable file. Speculative prefetch stays disabled until recorded scan traces
beat point reads under bounded provider latency, as required by the
[canonical scaling contract](canonical-ltx-scaling.md).

For writes, attribute SQLite command time, LTX capture, follower append
and fsync, root preparation, object CAS, queue wait, and final proof source.
The runtime already supports follower and object proofs. Tune batching or
publication only after traces show which wait dominates. Preserve the
[acknowledgement and recovery implications](failover-and-followers.md#preserve-these-guarantees):
every released result is covered, and a successor seals/replays any
follower-only tail before serving. Inject owner process and local-disk loss,
one follower loss, RustFS delay/failure, and ambiguous CAS.

**Proof commands for the existing focused suites:**

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-ltx --features replica --test cell \
  sparse_hydration_coalesces_contiguous_cell_frames --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-cell-runtime --features test-support --test runtime \
  resident_route_reports_zero_origin_reads_and_latency_percentiles --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-cell-runtime --features test-support --test runtime \
  restored_sparse_route_promotes_before_zero_origin_reads --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-cell-runtime --features test-support --test runtime \
  follower_proofs_advance_logical_head_and_bound_the_object_backlog --locked
```

These focused tests are a local correctness gate. The RustFS action command
above and the protected qualification workflows remain separate gates. Do
not claim a recovery percentile from one owner-loss sample.

## Work packet 5: prove application-level scale

Use the existing [reference application](application-framework-example.md)
and [Cell-backed issue example](../../crab-http-server/deploy/cell-issue-fleet/README.md).
The issue example proves gateway distribution, owner routing, and one Cell
per repository. Extend the reference application workload to exercise
entity, shard, workflow, and read-model Cells through public
`CellNode`/application handles and stable typed operations. One command
changes one Cell. Cross-Cell changes flow through durable effects,
deduplicated inboxes, and idempotent activities; no global SQLite
transaction or follower SQL read is implied.

The load matrix needs three shapes: many evenly distributed Cells, a
deliberately hot Cell, and skewed read/write Cells. At each of 3, 5, 10,
and 20 nodes, report per-node CPU, memory, disk, active Cells, queue depth,
object requests, and throughput curves over increasing client concurrency.
Separate owner-local, forwarded, sparse, resident, fleet-proof, and
object-proof actions. A fixed 20-lane rate is not the maximum throughput.
Every successful write must have a visible readback or durable receipt and
survive owner loss. Reject duplicate effects and regressions in published
root sequence.

Use the existing public-host cases as the first application correctness gate:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-cell-app --test reference_application \
  three_node_host_resolves_ambiguous_result_and_deduplicates_delivery --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-cell-app --test reference_application \
  three_node_host_recovers_published_state_after_owner_loss --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-cell-app --test reference_application \
  three_node_host_serves_two_release_ids_with_unchanged_module_contracts --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo test -p crab-http-server --test public_cell_takeover --locked
```

Run these before the scaled load, then repeat the relevant cases after any
change to routing, admission, VFS, or durability. Build and broad provider
proof belong in CI or a dedicated test environment.

## Release decision

The work is ready for a supported profile only when:

1. All focused safety tests and docs validation pass; the changed HTTP,
   runtime, LTX, and application surfaces pass their relevant CI gates.
2. Four stage reports and repeated same-profile 20-node runs show the
   expected routing reduction, stable latency distributions, no unexplained
   503s, and no RustFS file-descriptor or I/O-queue exhaustion.
3. A multi-host run repeats owner loss, follower loss, object-store fault,
   rolling release, and skewed load with isolated node disks and network
   failure domains. One-host Compose does not meet this gate.
4. Signed qualification receipts bind source revision, image, node profile,
   provider, workload, fault schedule, and raw artifacts as required by
   [delivery and qualification](delivery.md). Supported limits and SLOs
   come from those receipts, not from this plan.

Stop a work packet and report the first failing invariant if a successful
result disappears after owner loss, a stale owner executes a command, a
resident read reaches object storage, a follower-only acknowledgement lacks
a recoverable tail, or a new route skips product authorization. Record the
failing request ID, Cell target, owner epoch, proof source, and raw report
outside the checkout; do not weaken a test or baseline to proceed.

Run the documentation contract check after editing this file:

```sh
node crates/crab-cell-runtime/docs/validate.mjs
```
