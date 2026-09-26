# Plan 036: S3-durable Cell read replicas and fenced promotion

Status: IMPLEMENTED — named local RustFS paths and focused fault checks pass;
3/5/10/20-node smoke, five-node unequal-ingress load, reader loss, fenced
promotion, offline retention, and interrupted mode rollout are recorded.
Production provider, independent-host, capacity, and release gates remain open.
Base: `origin/main` at `de0bb234abc` (2026-09-25); Cell/LTX source compared with the planning checkout at `fa182c94c7e`
Priority: P1 read scaling; P0 safety for any enabled deployment. Effort: XL. Risk: HIGH.
Depends on: the recovery implementations tracked by plans 032 and 035;
production enablement also depends on their capacity and protected-evidence gates.

Execution provider scope: local RustFS only, as requested. Protected S3 and
dedicated multi-host release qualification remain future release gates.

> Executor: read root `AGENTS.md`, `crates/AGENTS.md`, and the scoped guides for
> `crab-ltx`, `crab-cell-runtime`, `crab-cell-host`, and `crab-http-server` before
> editing. Recheck this plan against current `main`. Implement one slice per
> reviewable change; update this status and the evidence table after each slice.

## Goal and decision boundary

One Cell has exactly one fenced writer. For selected read-heavy Cells, a
variable, admitted number of other physical nodes may serve **explicitly
replica-consistent** typed queries from verified, read-only snapshots. A
secondary may become the next writer only through the existing failed-session
recovery and Cell-control takeover CAS. No replica assignment, local SQLite
file, follower receipt, or route cache grants ownership.

For the requested all-secondary-loss guarantee, **every acknowledged mutation
must have an exact S3 root and Cell-control CAS before its response**. The
current default `fleet` mode can acknowledge after follower fsync before S3
publication. If the owner and all followers are lost in that interval, S3
cannot reconstruct the acknowledgement. Use the existing `object` proof mode
for this deployment profile; it disables log-follower recruitment. Read
secondaries are independent of durability followers. Do not call a fleet-proof
write tolerant of simultaneous loss of every witness. If the S3 root or
authority is unavailable, writes and promotion wait or fail closed.

Success is a three-node user action: acknowledge an S3-rooted write, read it
from two secondaries, lose both secondaries and the primary including their
local disks, recruit a fresh node from S3, read the exact acknowledged value,
and reject the old owner's next write. In a separate run, kill only the primary
and promote a warm eligible secondary. Vary the reader target across 0, 1, 2,
and more nodes, including loss and replacement. Measure read throughput,
freshness, memory, disk, descriptors, S3 calls, and takeover time before a
production claim.

The canonical scaling and follower documents now describe this opt-in
read-only extension. Durability followers remain logs; read secondaries have
independent placement and admission. The library client policy is explicit,
and the product peer route requires object durability mode.
The existing nine-profile protected release gate remains independent; local
Compose or RustFS receipts never satisfy it.

Implementation status at this revision: `crab-ltx` opens a verified root as
a sparse immutable SQLite view over authenticated pages, and `crab-cell-runtime` can execute a
typed query against that view with a fresh authority/session response gate.
`CellReadReplica::refresh` serializes refreshes, verifies the replacement root,
and switches a shared snapshot after a fresh authority check; in-flight queries
retain their exact old view until completion.
The S3 desired-count object supports conditional create/update. In object
durability mode, the server now reconciles active owner Cells, sends authenticated
activation hints to selected nodes, refreshes their admitted read views, and
accepts explicit authenticated private replica queries. An administrator may
CAS the target count through the product API, which sends an authenticated
owner wake-up hint. Its status route probes selected nodes and distinguishes
proven-ready, unverified, and placement-shortfall counts. An explicit issue-detail
route uses selected replicas and reports actual receipts without falling back
to the writer. It chooses the fewest outstanding attempts observed by this
ingress, rotating equal-load choices across readers. Counts span Cells and
router clones and are released on success, error, timeout, or cancellation;
they do not measure work issued by other ingress nodes. The selection and query
path now live in runtime `ReplicaReadRouter`. Hosts wire it into `CellClient`,
and authors explicitly choose `ReadPolicy::Replica` on an `ApplicationHandle`
before constructing generated clients. Commands, resolution, streams, and
primitive lease validation remain owner-ordered. Compiled SQL and primitive
namespaces may open snapshots through the same registry/code/schema checks;
the former repository-role restriction is removed. General product reads remain owner-only; production qualification remains open. Warm-reader
preference now probes verified snapshots after owner death, closes read admission,
and enters the existing fenced takeover and fresh writable restore path.
The server can select the existing object proof path with `[cells]
durability = "object"` for a fresh deployment; fleet remains the default and
a local offline rollout runner now verifies the fleet-to-object drain and
coverage barrier before changing configuration. Platform rolling upgrades
remain unqualified. Issue-detail replica reads are explicit; other public
product reads use the owner.
The local Compose fault receipt proves recovery after all three Cell disks are
lost in this object-mode fixture. It does not establish production durability
or throughput scaling; measured replica throughput is below owner throughput.

Today the durability-log ensemble has one follower in a two-node fleet and
two in a fleet of three or more. A failed member stops fleet proof for the
affected batch; the owner can complete via S3 object proof, then rotate the
ensemble only after all prior tickets are object-covered. This count is not a
read-replica target. This plan makes **read** replica count variable and does
not expand the durability-log ensemble, because S3 proof is the required
acknowledgement in the all-secondary-loss profile.

Amazon S3 documents strong read-after-write consistency and conditional
`If-Match` writes for one key
([consistency](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html),
[conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)).
`crab-storage::Store::update` uses `PutMode::Update(ETag)` and
`CellAuthority::transition` validates the complete successor before that CAS.
Use those existing S3-backed authority primitives; do not introduce a second
leader record. Validate the exact bucket/provider's conditional-write behavior
and failure responses before treating an S3-compatible emulator as equivalent.

## Current ownership and reuse map

| Boundary | Current owner and behavior | Change required |
| --- | --- | --- |
| Authority | `crates/crab-cell-runtime/src/control.rs`, `crates/crab-cell-runtime/src/control/authority.rs`: one owner/session/epoch, exact root, recovery overlay, ETag transition | None to the authority format or CAS rules |
| Durability | `crates/crab-cell-runtime/src/node/`, `crates/crab-cell-runtime/src/follower.rs`: default `fleet` mode uses one or two fsynced followers and may acknowledge before S3; `object` mode waits for exact root CAS | Select `object` mode for this profile; preserve fleet mode's existing, narrower failure contract for other users |
| Reads | `crates/crab-cell-runtime/src/client.rs`, `crates/crab-cell-runtime/src/client/local.rs`, `crates/crab-cell-runtime/src/cell/actor/handle.rs`: typed query runs FIFO on the owner and returns a Cell/incarnation/sequence receipt | Add an explicit replica read route; retain owner route for strong reads and resolution |
| LTX | `crates/crab-ltx/src/replica.rs`, `crates/crab-ltx/src/paged_io.rs`, `crates/crab-ltx/src/writable_vfs.rs`: authenticated exact-root pages, fresh writable sparse activation, and exact full restore | Add one read-only snapshot opener sharing verified page access; never open a capture-capable `Db` on a replica |
| Application | `crates/crab-cell-runtime/src/registry/builder.rs::execute_query` and `crates/crab-cell-app`: compiled query, schema, codec, and byte limits | Reuse the exact registry binding and authorization contract; no second query interpreter |
| Routing | `crates/crab-cell-runtime/src/peer/{dispatch,transport}.rs`, `crates/crab-http-server/src/{cells/router,peer}.rs` | Add authenticated replica query/status and advisory refresh; never route writes to a replica |
| Promotion | `crates/crab-cell-runtime/src/node/log_recovery.rs`, `crates/crab-cell-runtime/src/control.rs`, `crates/crab-http-server/src/cells/router.rs`: prove failed session, recover any prior active log, acquire new epoch, open exact root | In object mode, acquire the S3-pinned root through the existing takeover CAS; never promote by local vote |

## Fixed safety contracts

1. `serving_writer(cell) <= 1`. Only the current Cell-control owner and live
   node session can mutate SQLite, publish roots, release acknowledgements, or
   answer a `CurrentOwner` query.
2. In this S3-durable profile, `response(commit) =>
   s3_control_root_covers(commit)`. A fleet proof alone cannot release a result.
   Before rolling an existing fleet deployment to `object` mode, drain its
   owner sessions and prove every accepted fleet ticket is covered by the
   exact S3 root; close/retire active node logs through the existing barrier.
   No in-place durability-mode toggle is assumed. A missing witness or
   ambiguous S3 result blocks the rollout.
3. `replica_read(cell, s)` implies a verified immutable root once named by
   authoritative control for the same Cell/incarnation/code/schema, a local
   read-only SQLite snapshot at sequence `s`, and a fresh response gate that
   has not observed tombstone, incompatible release, or changed owner epoch.
   Replica reads may lag newer S3-rooted writes.
4. `After(receipt)` on a replica returns only at or above its sequence for the
   same Cell/incarnation. Behind, unavailable, or unprovable is an explicit
   error within the deadline; it never returns an older result. An application
   may choose the current-owner route separately.
5. `promoted(replica)` implies the predecessor session is proven expired, any
   predecessor log left from an earlier fleet-mode epoch is sealed/recovered,
   the candidate won the new-epoch control CAS, and a fresh writable SQLite
   session opens the exact S3 root before admission. A warm read snapshot alone
   satisfies none of these steps.
6. A replica's local bytes are disposable. No bucket listing, local timestamp,
   cached route, signed advertisement, or follower ACK selects the latest root.
   Offline retention drains read replicas before sweeping objects they might
   still reference; do not invent an online pin protocol for this plan.

## Read-replica design

### Placement and lifetime

Initially enable read replicas only for selected hot Cells in the S3-rooted
`object` durability profile. The desired reader count may change from zero to
the number of eligible non-owner physical nodes, subject to measured resource
admission and at most one readable copy per physical node. The product
operator owns one narrow target-count request per Cell; a bounded S3
conditional-write policy object persists that **desired count only**, not
replica assignments or authority. A missing policy means zero readers. This
separate object is justified because target count can change without a Cell
schema release or a writer-root CAS. The automatic demand policy comes only
after measured owner-query and replica-query telemetry. Do not hardcode
application Cell IDs or add an environment variable.
An operator target change CASes the policy object and sends an authenticated
hint to the current owner; the request reports applied or pending status.
After an ambiguous CAS response, reread the exact policy object. Define its
typed `CellStorageLayout` path as
`cells/v1/apps/<app>/cells/<cell>/read-policy.json` and a strict, bounded
version-one codec with Cell/incarnation, desired reader count, and revision.
Only the authenticated product operator may change the target; replicas may
read it but cannot write it. The owner rechecks target state after activation;
no listing or separate policy leader is needed. A different incarnation
rejects the old policy until an operator writes a new one.

One node-wide reconciler consumes the current target and the existing signed,
live node observations for Cells that this node currently owns. On activation,
the owner loads any target policy for that Cell directly; no bucket-wide LIST
discovers policies. A new owner resumes reconciliation after its takeover.
Exclude the owner, expired/draining/incompatible or
capacity-full nodes; prefer distinct failure domains, then rendezvous-rank by
Cell ID and stable physical node ID. Select the first admitted `N` candidates.
Candidate identity is advisory and is never stored in `control.json`. Each candidate
holds an in-memory readiness record keyed by Cell, incarnation, owner epoch,
root digest, commit sequence, code/schema, and local activation generation.
The host reserves measured memory, SQLite connection/descriptor, sparse-page
cache, disk, and refresh-work costs through the existing node resource ledger
before opening a view. Pressure shedding closes reader admission, waits for
in-flight queries, then releases the view and reservations. Reader eviction
cannot release the Cell owner. No query replica participates in the mutation
acknowledgement gate.

When a selected node dies or its signed session expires, the reconciler
removes it, selects the next eligible node, and sends an authenticated
activation hint. The replacement creates a fresh read-only view of the
authoritative S3 root; it never copies an unverified file from the failed
node. If no spare node has capacity, the actual count is below the target and
the operator status reports the shortfall. A node that rejoins must revalidate
selection and root/epoch before answering a query. The framework recruits
already-running nodes; Kubernetes or Docker Compose owns launching new Pods
or containers when the fleet itself needs more capacity.

### Snapshot refresh and query execution

The owner may enqueue a bounded authenticated **advisory** root-published hint
after its existing control CAS; acknowledgement never awaits replica refresh.
The replica re-reads Cell authority directly, verifies
the exact root graph via `CellReplica`, and builds a fresh read-only view. A
bounded reconciliation tick for **active read replicas only** handles lost
hints. Coalesce concurrent refreshes per Cell and charge them to the existing
I/O, disk, and blocking-work budgets. Never update the file currently used by
a query: atomically install the new view after verification, then close the old
view when its last query exits. Refuse sequence regression, a changed
incarnation, schema/code mismatch, missing authenticated pages, and partial
local files. The first implementation reads published roots only. This is
complete for the S3-durable profile because acknowledged writes cannot outrun
their S3 roots; replica refresh may still lag that root.

`crab-ltx` should expose a narrow read-only exact-root opener over its existing
authenticated page resolver. SQLite must open with read-only flags and enforce
the query-only boundary; no capture session, mutable WAL owner, or root
publisher is constructed. Root refresh creates a new view, not an in-place
patch. Use `Registry::execute_query` with the same compiled handler,
namespace/schema/codec checks, input/output limits, SQL interrupt, and
deadline as the owner query. Audit every primitive query with non-SQL external
reads before enabling it on replicas. Keep HTTP authentication and repository
authorization in `crab-http-server`.

The initial response gate performs a fresh authoritative control and owner
session check after SQL execution. It requires the same owner epoch,
incarnation, code/schema, and a live serving owner; tombstone, recovery,
drain, incompatible release, or an unavailable authority check fails closed.
This is intentionally conservative. Record the object-store calls and p99
cost; any later cached read-lease optimization needs its own revocation and
takeover proof. The gate's control read is the replica query's observation
point. It cannot claim linearizability with mutations; `CurrentOwner` remains
the only owner-ordered route.

### Client and peer contract

Keep the existing owner query as the default. Add one explicit read policy to
the canonical typed query path: `CurrentOwner` or
`Replica { minimum: Option<Receipt> }` (illustrative names). A replica route
spreads queries across ready candidates using measured in-flight load, tries
another eligible candidate only within the same absolute deadline, and
returns typed `ReplicaUnavailable`, `ReplicaBehind`, or `Fenced` outcomes when
none can prove the requested position. It does not silently fall back to owner
execution. `After(receipt)` remains available on the owner route for
read-your-write while secondaries are still refreshing.
Every successful query returns its actual observed receipt, not the requested
minimum. Validate Cell and incarnation as well as sequence on both sides of
the peer transport. Reuse the existing mTLS principal, target scope, registry
contract, and server-side product authorization; a read-only route is not an
authorization bypass.

### Promotion and failure behavior

Replica membership only influences the **preferred eligible successor**. A
failure detector first proves the owner session expired. In `object` mode the
current Cell control already names every acknowledged root; the existing
takeover path CASes a new epoch and opens that exact root. If the deployment
previously used `fleet` mode, its active old log must first pass the canonical
seal/overlay recovery path; a missing old witness cannot be skipped merely
because the new policy is S3-rooted. The successor may reuse an authenticated
warm page cache only if it proves the cache matches the exact root; it does
not convert a read-only SQLite file into a writable capture session. It closes
reader admission, wins the new Cell epoch by CAS, creates a fresh writable
SQLite session, and publishes readiness. Another candidate that loses the CAS
stays read-only or closes. The former owner is fenced on return. If every
secondary died, any eligible fresh node may restore directly from S3. If
authority CAS is unavailable, nobody promotes. Do not add a replica election
record, mutable LTX head, or owner-to-owner database copy.

## Implementation slices and exit evidence

Current slice state (local proof only): 0 reconciled in docs; 1 has
the existing object-proof path and a verified offline fleet-to-object rollout,
including interrupted drains after follower loss and provider outage;
2 sparse immutable opener, atomic refresh, exact-root isolation, bounded page
cache, cancellation cleanup, and provider fault tests pass locally; runtime
and product mTLS tests pass on local RustFS; 3 policy CAS, signed selection,
owner reconciliation, private activation, node admission, and administrator
target CAS with owner hint and bounded readiness status exist; local
0→1→2→4→1 convergence passes, including shrink during reader loss;
extended production churn qualification remains; 4 typed local and peer queries, position
errors, receipt checks, authority gates, and explicit issue-detail routing exist,
including ingress-observed in-flight load selection under one request deadline,
and explicit policy on typed/generated application clients, but other product
reads remain owner-only; 5 has warm-reader preference and
local automatic takeover coverage; 6 has a source/image-bound 3/5/10/20-node Compose run with reader replacement,
warm promotion, all-reader disk loss with a new acknowledged write, global
authority outage, and a separately isolated reader during takeover; broader capacity
and production fault qualification remain open. Private peer replica requests are accepted only in
the object-durability server profile.

The ignored `rustfs_replica_reads_exact_root_and_policy_cas` test also passed
against a local RustFS bucket with an isolated prefix. It exercised real S3
root reads, atomic snapshot refresh, fencing after release, and a policy
ETag update. It was one process, so it provides neither multi-node distribution
nor protected-provider evidence.
The local in-memory and RustFS tests now also prove that a failed refresh
leaves the old value readable and that an in-flight query returns its old
snapshot after a newer view is installed. Runtime Clippy passed with warnings
denied. These library checks complement the admitted product route and Compose proof below.
Replica SQL now shares the existing node SQL worker admission pool, retaining
its job reservation until blocking execution exits. Concurrent peer codecs
wait within a deadline on the existing primitive-job ledger; S3 enrollment
lookup no longer holds that CPU reservation. The live concurrency regression
reproduced immediate 503 rejection before this correction.
The reader now takes a node runtime on open. Each live snapshot reserves a
provisional 12 MiB of resident memory and four descriptors in that runtime's
ledger. The memory charge conservatively includes the entire shared 8 MiB
page cache and 4 MiB of view/fetch overhead. Its local placeholder is empty;
page bodies are not restored to disk.
Refresh reserves a second view until old in-flight queries finish. Local and
RustFS tests cover capacity rejection, concurrent charges, and full release;
the Compose receipt below samples resources under 1 GiB/1 vCPU limits.
Peak-resource and sustained-capacity qualification remain open.
At `04bf77cced9`, deterministic tests exposed and fixed two lifecycle gaps:
SQL-pool shutdown previously joined only dedicated workers, leaving replica
blocking tasks running; cancelling a query and dropping its reader released
view admission before that SQL exited. Shutdown now waits for the existing
SQL-job ledger to drain, and each blocking query retains its complete snapshot
and admission until execution finishes. The final job admission is serialized
with pool closure. Both regressions pass in memory and on local RustFS.
A real schema migration during a blocked old-view query now has explicit
proof: unchanged owner/epoch, a new code/schema root, rejection of the old
query and refresh, and the migrated value through a fresh reader. The complete offline retention sweep now passes as recorded below.
Interrupted offline mode rollout now passes; platform rolling upgrades and
exhaustive release-fault interleavings remain separate production qualification.
An explicit local or peer replica query whose view is behind a caller's minimum
returns `ReplicaBehind` with both sequence numbers. The private peer wire has
an explicit read operation and distinct behind/unavailable error codes.
The sparse opener runs under SQL-job admission independently of LTX's blocking
I/O pool; otherwise a cold directory-cache read can deadlock an opening view.
Admission and cleanup stay with dispatched work after waiter cancellation.
`cancelled_read_view_install_removes_its_unclaimed_destination` pauses after
fresh placeholder creation and proves removal after cancellation. The separate
full-restore path retains its exact no-clobber install contract.
Read-only views now apply the same 64 KiB page-cache target and disabled
lookaside allocation as managed LTX connections; the exact-root view test
checks the SQLite cache setting.
`desired_readers_follow_live_distinct_nodes_and_replace_a_lost_member` proves
the advisory directory selection at targets 0, 1, 2, and 4, with distinct
physical nodes, failure-domain preference, an expired reader, and a new live
replacement. A candidate advertising less than the provisional reader-memory
reservation is excluded. A server reconciler now activates selected readers
through authenticated hints, and each receiver verifies policy and reserves
its node resources before opening the exact S3 root. The existing two-node
product E2E passes with both in-memory storage and local RustFS: the non-owner
reads the written issue from an admitted replica through the signed private
mTLS peer route, the policy change to zero releases reader admission, and the
old view is fenced after the owner epoch changes. This is one-process test
wiring, not the required independent Pod qualification.

The earlier uninterrupted local Compose run used runtime source `75b6da1a97d`,
runner `f8b164094e3`, and image
`sha256:921dc5c7ec9ba004df65430a03a8cb9f1a0957e62a253f4578c876ed4e66b2ce`.
At 3/5/10/20 nodes, all 2/4/9/19 selected readers served correct issue details;
200 concurrent reads per stage distributed as 100, 50, 22–23, and 10–11 per
reader, with no request errors. A killed reader was replaced without changing
the writer or epoch. Primary-only failure promoted a verified warm reader
and acknowledged a new comment. Deleting the owner's and both readers' local
volumes recovered the identical root and recruited two replacements. A RustFS
outage returned `replica_unavailable` without data, then recovered without
receipt regression. RustFS descriptor admission was explicit and verified.

[Source-bound measurements and fault receipts](../crates/crab-http-server/deploy/cell-issue-fleet/qualification/2026-09-25-read-replicas.md)
record timings, per-node distribution, sampled resources, raw-report hash,
and limits. Replica reads were slower than owner reads in this single-host
workload at that revision; membership scans and fresh authority gates were in
each request path. No production capacity, throughput-scaling, protected-provider, or
independent-host claim follows from this receipt.

A second fixture verified the offline fleet-to-object transition at runtime
`997082d38eb`, runner `1f77d6a478d`, image
`sha256:c8ae5fda425560b34939631f7fd2a101264224cb485cb1c7c01e5d20d2b436af`.
It observed five fleet proofs, drained all three servers with successful exits,
switched to object mode, and verified all 37 pre-switch comments plus a new
object-mode comment after deleting every local Cell volume. Two readers were
recruited again. The runner retains fleet configuration on any failed drain.
The host shutdown ordering was corrected after a real failure: work producers
stop first, runtime publication and log close finish with live heartbeats, and
session withdrawal follows. Deadline and task-limit regressions pass.
The linked qualification receipt records both image boundaries and report hashes.

| Slice | Change owner | Implementation and focused gate | Completion evidence |
| --- | --- | --- | --- |
| 0. Reconcile contracts | `crates/crab-cell-runtime/docs/{canonical-ltx-scaling,failover-and-followers,application-framework,deployment}.md`, product durability config | Record this as an opt-in extension to the former read-replica non-goal. Freeze S3-rooted acknowledgements, target-count ownership, `CurrentOwner` versus explicit replica-read semantics, and public API/peer version. Check release tags before changing any shipped wire or API shape. | Reviewed invariant and compatibility map; fleet-only acknowledgements are excluded from the all-secondary-loss claim. |
| 1. S3 proof and mode rollout | `crates/crab-cell-runtime/src/cell/actor/requests.rs`, `crates/crab-cell-runtime/src/node/{durability,log}.rs`, server composition and tests | Use existing `object` proof mode for this deployment; enforce `ack => exact S3 root CAS` for successful mutations, durable rejections, and state-observing outputs. Before a fleet-to-object rollout, drain old sessions, wait for object coverage of every accepted ticket, then retire the old log by its barrier. Inject lost CAS response, S3 outage, and total follower loss at each phase. | No acknowledged cut exists only on nodes; an incomplete rollout remains in fleet semantics and cannot claim all-loss tolerance. |
| 2. Read-only root view | `crates/crab-ltx/src/{replica,paged_io,writable_vfs}.rs` or a narrowly owned sibling; LTX suites | Implement fresh read-only SQLite open over verified root pages, atomic view replacement, bounded page/cache/disk ownership, and interruption-safe close. Test root A/B, concurrent old-view query, corrupt page, missing object, failed refresh, source deletion, and no writable handle. | Querying A while B is installed returns A's exact bytes; new queries return B; corruption never yields a row; no mutable LTX or Cell control write occurs. |
| 3. Dynamic reader placement | `crates/crab-cell-runtime/src/fleet/resource.rs`, `crates/crab-cell-runtime/src/node/directory.rs`, server reconciler, and S3 target policy | Reconcile desired 0→1→2→4→1 readers from signed live membership; exclude owner, choose distinct physical nodes, admit through the existing ledger, and replace dead readers from the S3 root. Coalesce refreshes and bound workers/disk. | Actual ready count converges or reports a typed capacity shortfall; no duplicate physical node, unverified source copy, leaked descriptor/disk, or write-ack dependency on readers. |
| 4. Explicit routing | `crates/crab-cell-runtime/src/client.rs`, `crates/crab-cell-runtime/src/peer/`, `crates/crab-http-server/src/{cells/router,peer}.rs`, product authorization tests | Add one explicit replica route, authenticated advisory refresh hint, mTLS read dispatch, receipt validation, and fail-closed final authority gate. Preserve the owner query's current behavior. | Owner read is ordered; replica read reports its actual older position; `After(receipt)` on a lagged replica never returns an older value; unauthorized, stale-epoch, and tombstoned reads fail. |
| 5. Fenced promotion | Existing `crates/crab-cell-runtime/src/control.rs`, session authority, server activation; takeover tests | Prefer an eligible warm reader, but always prove predecessor death and win the epoch CAS before fresh writable restore. If all readers died, recruit any eligible new node from the S3 root. Preserve old-log recovery for pre-switch fleet epochs. | S3-rooted acknowledged write survives loss of owner and every reader disk; only one successor writes; returned old owner cannot write. |
| 6. Live qualification and capacity | Existing Compose/Kubernetes harnesses, qualification profiles, metrics, deployment runbook | Exercise 3→5→10→20 distinct nodes with real RustFS side effects and fault injection. Record target/ready count, replacement time, actual per-node reads, freshness lag, control GETs, refresh bytes, reader RSS/disk/FDs, and promotion time; run protected S3 provider and 1k/5k/10k admission proof before a release claim. | Raw receipts bind exact source/image/profile; no existing threshold is weakened; all-reader-loss and S3-root promotion pass under independent Pods/providers. |

After each slice, update the whole call path's docs and tests. Do not add a
second query registry, authority path, follower log, resource ledger, or
unbounded per-Cell background task. Before broad optimization, compare measured
cost against owner-only reads on the same workload and proof mode. If replica
reads do not improve a measured bound, keep the feature opt-in and report the
cost rather than weakening safety.

The Compose reader qualifier now retains per-node before/after control-load
and LTX-fetch counter series around owner reads, replica reads, and an
acknowledged issue-body refresh. It records sequence lag and each selected
reader's first observed fresh result. These whole-node windows include
background work and exclude uninstrumented membership/policy calls and
provider-internal retries; they are not total billable S3 requests. Routing now
reuses the host's instrumented authority instead of silently dropping its
control-read telemetry. A fresh-image 3/5/10/20-node run at `d6904ba187de`
passed with these measurements and the sparse views. Five-node replica
throughput was 411.72 requests/s with exactly 50 of 200 reads per secondary;
the same workload's owner route achieved 3,051.56 requests/s. Separate
follow-ups proved a new S3-rooted write after deleting every original Cell
disk and the explicit target-count sequence during reader loss. The linked
qualification receipt preserves each runner/image boundary and report hash.
A further local S3-path partition left one reader without authority while a
healthy node recovered the unchanged root, acquired the next epoch, and
acknowledged a new mutation. The isolated node first returned typed
unavailability, then closed its public listener after its session expired;
it never became primary. This is separate-process, single-host evidence.

The local offline-retention case now passes on source `287a5eb1397`:
three serving nodes drain, a one-object sweep remains fenced in Maintenance,
and the same prepared revision resumes to Ready with a successful process exit.
The run deletes 5,456 aged unreachable objects, preserves both backup pins,
recovers all twenty issues and comments, recruits two readers, and acknowledges
a new write. It exposed and fixed maintenance-session reuse after tombstoning
and runtime shutdown bypassing its owning host. The linked qualification
receipt distinguishes the failed attempts, fixed image, and successful run.

Additional local proof now covers both readers dying before a surviving
writer acknowledges a new S3-rooted value, followed by loss of that writer's
disk and recovery at the next epoch. A two-minute five-node workload through
unequally loaded ingresses returned correct values without errors and balanced
4,345 replica reads across four readers (1,084–1,090 each). Replica throughput
was 72.39 requests/s versus 279.10 for the owner route. Concurrent refreshes
coalesce; storage-full snapshot creation fails without replacing existing
files and can retry. The deterministic durability suite now blocks drain of
a follower-only acknowledgement until exact S3 publication, also when that
root CAS commits but its response is lost. The interrupted offline rollout
runner observes fleet proofs, refuses configuration change after loss of both
followers or RustFS, recovers every acknowledgement, and then completes a
healthy switch followed by loss of all three local disks. Its nineteen fleet-era
comments and one object-mode comment survive; two readers are recruited again.
See the linked source-bound receipt.

The subsequent reader-discovery optimization shares one bounded one-second
membership observation, reuses the owner's signed immutable boot identity for
placement, and overlaps independent control/policy loads. The post-SQL
authority/session gate remains fresh. Three five-node RustFS pairs at runtime
`c9d16ebeb358` passed a fixed 75% node-1 / 25% node-5 request mix with the
primary held on node 5: replicas delivered 858–1,049 requests/s versus
412–450 for the owner route, with 6.70–7.33 ms median latency versus
20.27–21.07 ms. Every pair met the 80% throughput / 120% median and p99
comparison thresholds. Local-primary reads remained faster at about 1.9 ms;
the result applies to the specified fleet traffic mix. Pausing RustFS still
returned HTTP 503, then recovered the acknowledged value and sequence 504.
The linked receipt records the corrected baseline, exact images, per-ingress
results, and raw-report hashes.

## Fault matrix and release gates

The deterministic runtime/coordination suite must cover: target count changes
during membership churn; two replicas race to refresh; root CAS response lost;
S3 outage before and after root CAS; all readers die before the owner; owner
and all readers die together; an old fleet-only ACK during an attempted mode
switch; reader disk full or corrupted; reader partitioned from authority;
stale hint after epoch change; primary returns after takeover; release/schema
migration while old replica queries run; reader restart with stale files; and
offline retention after reader drain. Assert exact values and control
owner/epoch/root at each step, not just HTTP status or log text.


Local evidence map (the receipt records provider and process boundaries):

| Required case | Evidence |
| --- | --- |
| Target changes and member loss | Compose 0→1→2→4→1 follow-up; `tests/fleet/read_placement.rs` |
| Concurrent refresh and old snapshot isolation | `tests/runtime/lifecycle/read_replica/lifecycle.rs`; root A/B and in-flight query checks in `read_replica.rs` |
| Root CAS lost response and provider failure around publication | Ten `tests/runtime/lifecycle/durability/proofs.rs` cases, including publication-gated drain |
| All readers die before writer; all disks lost together | Reader-first loss receipt; full Compose all-loss receipt |
| Fleet ACK during mode switch | Deterministic blocked publication/drain; interrupted Compose rollout after follower/provider loss |
| Reader disk full, corrupt object, stale files | LTX `tests/host/hooks/restore.rs` and `tests/cell/roots/sparse.rs` |
| Reader authority partition and replacement | Isolated-reader Compose receipt; global RustFS outage receipt |
| Stale epoch, returned owner, schema change | Product private-peer takeover E2E; runtime authority gates and blocked-query schema-migration test |
| Reader drain before retention | Aged-object offline retention receipt with partial-sweep retry, backup verification, and fresh readers |

These receipts cover the named local cases. They do not establish every fault
interleaving or replace protected production release gates.

Local E2E must use the public typed application through separate processes:
one primary and two read secondaries, an S3-rooted acknowledged write,
replica-position reads, target changes with automatic replacement, `SIGKILL`
and deletion of every node's local disk, and a query/write from a fresh
promoted node. A separate primary-only failure must prefer a ready reader.
A network-partition case must show an isolated secondary cannot self-promote.
Read-distribution evidence must count actual replica query executions per
physical node, not only gateway request counts. Test 1 GiB/1 vCPU nodes
under their real Docker limits, then run dedicated-host scale and provider
qualification independently. RustFS supplies local S3-compatible smoke
evidence; the protected S3 run must verify the provider's conditional writes,
read-after-write, conflict classification, and lost-response reconciliation.
The current 20-node/20-Cell single-host Compose receipt is a starting smoke
test, not this feature's performance evidence.

Before any Cargo command, verify `$HOME/Workspace` is mounted and writable;
use a target directory unique to the executor's checkout. For this checkout:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo test -p crab-cell-runtime --features test-support --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo test -p crab-cell-app -p crab-cell-host -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-cell-app -p crab-cell-host -p crab-http-server --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
python3 crab/scripts/check-cell-ltx-layout.py
git diff --check
```

Run focused suites after each slice; broad suites and protected provider/Pod
qualification run in CI or a dedicated environment. Inspect any Compose
script's literal `target/` lookup before execution, and bind its image to the
same source. Reuse the fresh-process protected-bundle verifier rather than
creating a replica-specific signing shortcut.

## Stop conditions

Stop the affected slice on a returned value newer than its verified snapshot,
an older-than-requested receipt, a stale replica serving after authority
rejection, dual writable owners, a follower-only acknowledgement in the
S3-required profile, a missing pre-switch fleet tail, root/sequence rewind,
uncharged reader resources, or a new publication/election protocol.
If exact-root read-only SQLite cannot be implemented without a second mutable
owner, keep owner reads and report the blocker. If source-bound provider or
capacity evidence is unavailable, mark implementation proof separately and
leave production qualification open.
