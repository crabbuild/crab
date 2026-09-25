# Complete and qualify canonical Cell LTX scaling

Crab will finish production scaling on one canonical persistence path:
`Db` captures SQLite, `CellReplica` prepares immutable roots,
`CellRuntime` owns execution and durability, and `crab-http-server` composes the
product. The older standalone epoch-head, paged, and scheduler surfaces were
present in release tag `v1.2.4` but are now hard-removed under the recorded
compatibility decision; their stored prefixes are never interpreted as Cell
roots.

| Document intent | Value |
| --- | --- |
| Content type | Target design and delivery contract |
| Audience | `crab-ltx`, `crab-cell-runtime`, and `crab-http-server` contributors |
| Goal | Bound canonical Cell resources and qualify production scale/failover on one native Rust path |
| Status | In progress; implementation slices are tracked in `advisor-plans/004`–`017`; standalone hard removal is executed |
| Scope | LTX preparation, authenticated metadata, resident lifecycle, resource accounting, qualification, and standalone-contract consolidation |

[Back to the Cell runtime index](README.md)

## Make one path canonical

The production dependency and authority path is:

```text
crab-http-server RepositoryCellRouter
  -> crab-cell-runtime CellRuntime
    -> CellExecutor + CellPublisher + CellAuthority
      -> crab-ltx Db + CellReplica
        -> crab-ltx CellStorageLayout
```

Each module has one responsibility:

| Module | Responsibility | Explicitly does not own |
| --- | --- | --- |
| `Db` | Exclusive SQLite session, WAL capture, checkpoints, retained local cuts | Remote authority or response release |
| `CellReplica` | Verify and prepare immutable Cell roots, sparse reads, restore, compaction | Mutable owner or root publication |
| `CellExecutor` | Serialized SQL, request ledger, captured mutation outcome | Object-store authority |
| `CellPublisher` | Immutable preparation, ordered publication, ambiguous-result reconciliation | Owner selection |
| `CellAuthority` | Sole mutable owner, epoch, recovery overlay, and root CAS | SQLite or LTX parsing |
| `CellRuntime` | Activation, admission, durability gate, recovery, drain, and local lifecycle | HTTP authentication or provider construction |
| `crab-http-server` | Product routing, authorization, peer transport, deployment, and composition | LTX parsing or alternate publication |

`CellReplica` writes immutable objects and never changes mutable authority.
`CellAuthority` remains the only module allowed to publish a prepared root into
Cell control. The only legal mutable publication chain is:

```text
CellPublisher
  -> Control::publish_prepared
    -> CellAuthority::transition
```

Production server code must not call `crab-ltx` directly. Test fixtures may use
the server's `crab-ltx` development dependency to construct exact inputs, but
product behavior crosses the `crab-cell-runtime` interface.

## Preserve intentional redundancy

This design removes duplicate ownership, not useful independent mechanisms.

- `Db` and `CellReplica` are not competing paths. One owns local SQLite
  capture; the other owns immutable remote mechanics.
- `CellReplica` and `CellAuthority` are not competing roots. One prepares an
  immutable proposal; the other conditionally publishes the sole authoritative
  successor.
- Object-store durability and follower durability are intentionally independent
  proofs. Either may release a response, but object publication continues in
  order and remains long-term recovery authority.
- Logical and published heads are bounded monotonic watermarks around one SQLite
  writer and one publisher. They are not independently writable histories.
- Followers store recent verified LTX tails. They do not run writable standby
  SQLite databases or serve reads.

The runtime must continue to enforce:

```text
response(commit) => object_root_covers(commit) OR fleet_covers(commit)

fleet_covers(commit) => every selected follower fsynced the commit ticket

cell.serving => control names this owner and exact root
             AND every attached recovery overlay was consumed
```

## Meet these design goals

1. Bound publication memory by configured buffers, not database or capture size.
2. Bound authenticated metadata memory and local cache disk across 10,000 open
   Cells.
3. Evict quiescent Cells without losing any acknowledged result or reopening
   unverified mutable state.
4. Account SQLite, WAL, retained LTX, sparse pages, follower tails, immutable
   cache, Git/LFS staging, and full-job scratch under one node envelope.
5. Prove exact recovery and continued publication through process, disk,
   follower, network, and object-store faults.
6. Drive production coordination and deterministic simulation through the same
   sans-I/O decision kernel, then model-check its safety invariants at small
   scale.
7. Make a fully hydrated resident read route and execute with zero object-store
   operations; keep cold activation and durability proof costs explicit.
8. Rebalance quiescent ownership with live, signed resource observations,
   hysteresis, bounded movement, and cgroup-aware pressure shedding.
9. Produce signed capacity receipts tied to one source revision, image, node
   profile, provider, workload, and fault scenario.
10. Preserve every still-relevant safety test while consolidating the removed
   standalone replication contract into Cell proofs.

The following are not goals:

- A V8, JavaScript, WebAssembly, dynamic-library, or public primitive host.
- A second mutable SQLite owner, read replica, or hot SQL standby.
- A fallback from Cell roots to standalone epoch heads.
- Listing local files or object prefixes to infer the latest state.
- Reusing an old mutable SQLite file merely because it exists locally.
- Maintaining a second replication protocol beside the canonical Cell path.
- Claiming Celld performance superiority without matched measurements.
- Treating a route cache, placement plan, or fleet sample as ownership
  authority.
- Hot migration of a writable SQLite process or direct owner-to-owner transfer.
- A central mutable scheduler whose loss can stop safe request routing.

## Close the three major architecture gaps

This plan adds three first-class workstreams beyond LTX allocation and local
lifecycle. They are gaps in the current canonical Crab path, not evidence that
the path should be replaced.

| Gap | Current Crab evidence | Target architecture | Required proof |
| --- | --- | --- | --- |
| Protocol assurance | `Control` retains pure persistent transitions. The runtime now has a private coordination state machine, deterministic simulator, and pinned TLA+ model; async adapters carry activation generations and typed per-effect intents/IDs while parity coverage is still expanding. | One private sans-I/O coordination kernel used by production and simulation, a replayable adversarial scheduler, and a TLA+ model of the same durable state machine. | Pinned seeds find deliberately broken variants; model configurations check single-writer and acknowledged-durability invariants; remaining work is full decision extraction/parity, not a second policy path. |
| Warm request latency | `RepositoryCellRouter::route_existing` first asks the actor-owned resident lookup; sparse activation receives bounded background `Db::hydrate_step` work on the existing SQL worker. The zero-origin post-promotion qualification is still outstanding. | Actor-owned resident lookup before remote metadata, plus bounded background hydration. A fully hydrated local read performs zero object-store operations from route through SQL result. | An instrumented store observes zero calls for qualified resident reads; cold, sparse, hydrating, resident, local-write, fleet-proof, and object-proof latency are reported separately. |
| Fleet balancing | Signed versioned placement observations carry measured node headroom, Cell/job counts, and three backlog counters. The private server loop plans bounded transfers, the actor confirms exact settled releases, and the receiver restores through ordinary authority acquisition. Ownership counts now balance by weighted share beside the material headroom-gain path: one elected donor per complete snapshot, a two-percent receiver deadband, and batch, surplus, and room bounds. Cold activation also sends one authenticated hint to a preferred live node. Local, planner, and process race tests cover exact-root preservation, stale-owner fencing/recovery, donation without headroom gain, refusal to mix pre-batch counts, convergence at target, and failed receiver rollback; protected multi-process movement proof remains. | Deterministic weighted placement over signed live capacity, actor-approved quiescent release, idle eviction, cgroup-aware pressure tiers, hysteresis, and paced drains. Placement remains advisory; existing control CAS remains authoritative. | Skew, membership change, stale samples, pressure, receiver death, rolling drain, and oscillation tests preserve authority and converge within declared movement and latency bounds. |

The Celld comparison is pinned to upstream commit `10cb1303dac710dcb3b557e318e08c855261f68b`.
Its documentation reports about 1.1 ms p50 and 7 ms p99 for one fixed-host
warm resident request. Those numbers are a comparison baseline, not a Crab
measurement or an unconditional acceptance threshold. Celld also documents a
pure decision core, seeded simulation, small-state specification, weighted
ownership balancing, idle eviction, pressure shedding, and paced drains. Its
current balancing limitation is equally relevant: Cells are weighted by node
capacity but counted uniformly rather than by measured per-Cell CPU or memory.
Crab should close the assurance and routing gaps, then exceed that placement
model without weakening its exact-root and dual durability proofs.

The current-code evidence map is:

The standalone compatibility boundary is recorded in
[standalone-replication-audit.md](standalone-replication-audit.md). Its
decision is HARD REMOVE; the public standalone surface has been deleted while
shared authenticated mechanics remain private to Cell roots.

| Surface | Current owner and behavior |
| --- | --- |
| Request entry | [`RepositoryCellRouter::route_target`](../../crab-http-server/src/cells/router.rs) calls `route_existing` twice around an activation lock, then repeats catalog and control loads for activation. |
| Metadata lookup | [`CellCatalog::lookup`](../src/cell/catalog.rs) loads the shard head and every referenced immutable catalog page; [`CellAuthority::load`](../src/control/authority.rs) separately reads exact control. |
| Local residency | [`CellRuntime::resident_handle`](../src/cell/actor.rs) asks the actor for a fully resident owner before remote metadata; [`local_handle`](../src/cell/actor.rs) remains the verified slow-path lookup for sparse or activation callers. Fenced, draining, and non-resident actors miss safely. |
| Sparse hydration | [`Db::hydration` and `hydrate_step`](../../crab-ltx/src/db.rs) are driven by the actor's bounded hydration tick through the existing SQL worker; cancellation/restart and post-promotion zero-I/O qualification remain. |
| Fleet observation | [`NodePublisher`](../../crab-http-server/src/peer.rs) signs short-lived measured capacity and backlog observations; `NodeAdvertisement` carries a versioned placement signature. [`RepositoryCellRouter`](../../crab-http-server/src/cells/router.rs) plans movement from live signed samples and actor-settled candidates, then records confirmed release and receiver activation separately. Advertised disk headroom is clamped by the runtime ledger, server memory resolves nested cgroup-v1/v2 membership, and cold activation sends a bounded direct-node hint before normal authority acquisition. The test-only process race covers one shared-control winner; unified process-wide probe parity and protected multi-process movement proof remain. |
| Existing rendezvous | [`preferred_scanner`](../src/fleet/scheduler.rs) elects a catalog scheduler scanner. It does not rank or move Cell owners. |
| Transition safety | [`Control`](../src/control.rs) validates named single-record transitions; [`coordination.rs`](../src/coordination.rs) allocates and retires typed per-effect intents/IDs, while the actor fences completions by activation generation and effect family, drains the kernel-owned pending-effect set before fenced deactivation, and keeps effect timing coupled to the production publisher. Background hydration, renewal, persisted-work inventory refresh, drain, and shutdown pass queue/publisher/lease observations through the same kernel schedule transition before an adapter starts work. |

### Closed-book LTX telemetry and the prefetch gate

Capture telemetry is a fixed-size per-batch ledger. It attributes schema checks,
WAL existence and position resolution, WAL reads and page collection, encoding,
local writes, file sync, parent sync, verification, and checkpoint time. The
same ledger records logical WAL work, physical WAL file/read bytes, allocated
image bytes, finite read/snapshot strategy counters, and checkpoint runs, busy
outcomes, frames, backfill, and restarts. `Db` emits the ledger for both
successful and failed capture attempts; publication is not a second reporting
boundary. None of these observations is persisted or participates in authority,
checkpoint, or recovery decisions.

Replica telemetry crosses into `crab-cell-runtime` only as the closed enums
`LtxPhase`, `LtxReadOrigin`, and `LtxRequestOutcome`. Logical reads are counted
separately from provider attempts. Provider attempts retain succeeded/failed
outcomes and bytes returned before failure for cold, sparse, and hydrating
reads; resident reads increment only the logical counter and perform no
provider operation. The runtime exports phase result/duration and these finite
counters for cold, sparse, hydrating, and resident reads.
Cell IDs, paths, object keys, digests, and arbitrary caller strings cannot be
labels. Root-open, authenticated-directory, frame-fetch, ordered restore-write,
and compaction paths report success and failure through the same host hook.

Exact-root compaction downloads every selected authenticated body once into
scratch in bounded 1 MiB chunks. The same admitted spool supplies frame decode
and output encoding, eliminating the previous hash-verification pass followed
by a second provider read. Source-body bytes are included in scratch admission;
body BLAKE3, frame hash, page number, page checksum, final LTX checksum, and
no-clobber publication checks remain unchanged.

B-tree-guided speculative prefetch remains disabled until traces collected by
these counters demonstrate a scan workload whose p95 improves without
regressing the existing point-read contract. The current baseline already
coalesces one authenticated 64-page window: `cold_open_and_restore_improve_p95_under_object_latency`
proves one body request for a point fault under injected latency, and
`sparse_hydration_coalesces_contiguous_cell_frames` proves fewer range requests
than hydrated pages. A future predictor is acceptable only when all of the
following are verified:

- malformed SQLite pages produce no prediction;
- prediction changes fetch timing only, never exact-root authority checks;
- point reads issue no additional provider request;
- speculative workers, bytes, and cache residency use existing host admission;
- scan p95 improves on recorded workloads at 5/20/100 ms provider latency.

### Implementation evidence and remaining qualification

The first implementation slices now have one code path each: the architecture
guard rejects production `crab-ltx` imports from `crab-http-server`; the actor
uses a private coordination state machine for admission, scheduling, fencing,
publication, renewal, migration, inventory refresh, and drain, while the kernel allocates and retires typed
effect intents/IDs and the actor fences completions by activation generation
and effect family before fenced
deactivation; the deterministic simulator and bounded TLA+ model exercise the same lifecycle
predicates; the simulator's movement release also passes queue/publisher
observations through the same deactivation gate; resident-only lookup is
actor-owned and attempted before
catalog/control I/O; sparse restored Cells receive bounded
`Db::hydrate_step` work on the existing SQL worker; active-cell admission
uses an exact RAII resource ledger (including resident native bytes, active-Cell
file-descriptor reservations, bounded SQL-worker, hydration-job, and primitive
activity/effect reservations, with runtime metrics for hydration and descriptor
usage/capacity); the runtime installs a weak
ledger admission on `crab_ltx::DiskBudget`, imports existing local bytes, and
keeps LTX reserve/resize/release usage identical to the advertised disk total;
persisted
Queue/Workflow rows are re-inspected after durable work before eviction; the
eviction seam now has a pure
deterministic selector that
excludes unsafe obligations; directory-cache files are restart-persistent,
verified, and charged to the shared local-disk budget, while the HTTP startup
path inventories every regular file in prior process session directories and
holds those bytes in the same budget, rejecting symlinked or special layouts;
native and bundle
publication share the authenticated LTX inspection path with replayable scratch
sources; placement/pressure decisions are pure fixed-point functions with
versioned signed observations whose advertised disk total and free headroom are
reconciled with the runtime ledger; placement's bounded `u32` wire projection saturates
host-sized counters rather than wrapping; and qualification receipts are signed, bounded,
artifact-bound records. The `qualification_receipt` binary verifies exact
source, image, and artifact identity, and the release workflow consumes only
that bound evidence. The isolated local RustFS LTX, Cell takeover/retention,
HTTP collaboration, native-push, and receive-fault qualifications now pass;
they are provider evidence, not release receipts. Matched warm-restart
zero-origin latency receipts, complete advertised/metric parity, multi-process
movement, and protected Kubernetes faults remain release gates. The
standalone-surface decision is recorded and its execution is in this change.
The cold-activation planner seam
and its local receiver-failure rollback are implemented locally: a failed
rooted idle acquisition or fenced-owner takeover releases the takeover through
the canonical publisher path and leaves the exact root unowned. A pinned
recovery overlay remains owned until its follower proof is replayed and sealed.
The multi-process movement, membership-loss, and fleet-convergence receipts
still belong to the protected qualification gate.

The canonical native publication path now has its dedicated multi-GiB receipt:
the release `rustfs_cell_replica_scale_load` example grew a 5,368,709,120-byte
incompressible SQLite source through 160 bounded captures (320 immutable
segments) against RustFS 1.0.0-rc.1, deleted the source, restored the published
root, compacted the complete range, restored the compacted root, and matched the
source BLAKE3/length exactly. `/usr/bin/time -l` recorded 1,496.96 seconds wall
time and 592,805,888 bytes maximum resident set size (~565 MiB); the largest
observed compaction scratch LTX was about 5.1 GiB on the external qualification
volume. This closes the native 5 GiB/RSS publication gate; provider matrices and
protected fleet receipts remain separate gates.

The local warm-path regression
`resident_route_reports_zero_origin_reads_and_latency_percentiles` runs 64
resident-handle plus SQL reads after activation through an instrumented
`Store`; the latest run recorded p50 67us, p95 90us, p99 364us, max 364us,
and zero origin reads. It is intentionally labeled local evidence rather than
a matched-hardware or signed release receipt.

The companion
`restored_sparse_route_promotes_before_zero_origin_reads` publishes and drains
a Cell, reacquires its exact root through a new runtime, waits for verified
sparse hydration to promote the resident route, and observes zero origin calls
on the subsequent SQL read. This closes the local warm-restart regression seam;
provider-scale and signed release receipts remain separate gates.

The upstream comparison is supported by Celld's pinned
[`docs/testing.md`](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/docs/testing.md),
[`crates/logic/rebalance.rs`](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/crates/logic/rebalance.rs),
and
[`docs/limitations.md`](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/docs/limitations.md).

## Deepen existing modules instead of multiplying surfaces

The design adds implementation behind three narrow interfaces:

| Module | Interface | Hidden implementation and leverage |
| --- | --- | --- |
| Coordination kernel | Step from one explicit state and input to one decision | Lifecycle predicates, fencing, durability release, recovery, timers, and movement stay local. Production and simulation gain the same behavior without learning its internal branches. |
| `CellRuntime` resident lookup | Resolve one target to a current local handle or a miss | Actor map, admission generation, node lease, owner epoch, root, hydration class, and invalidation stay local. Routers do not assemble a second cache policy. |
| Placement planner | Rank eligible nodes and propose bounded actions from one signed snapshot | Resource normalization, weighted rendezvous, deadband, cooldown, pressure tiers, and movement budgets stay local. Execution still crosses the existing actor and authority interfaces. |

These modules pass the deletion test: deleting any one would spread the same
rules back across production routing, simulation, pressure handling, and tests.
They are private by default. The effect seam is real because production and the
simulator provide different adapters. The resource-probe seam is real because
Linux cgroup and portable process/host adapters differ. Do not add a public
route-cache trait, ownership-provider trait, or generic planner framework for a
single adapter.

The interface is also the test surface. Protocol properties enter through the
coordination step, route tests enter through `CellRuntime` lookup, and placement
tests enter through a complete signed fleet observation. Tests must not reach
past those seams to mutate internal maps or manufacture authority.

## Make coordination deterministic

### Extract decisions, not storage abstractions

Keep the existing storage implementations and public interfaces. Add a private
`coordination` module inside `crab-cell-runtime` that accepts plain immutable
observations and returns decisions. It owns no Tokio handle, object-store
client, filesystem path, SQLite connection, wall clock, random source, or
network client.

The kernel covers decisions that currently span `actor.rs`, `publication.rs`,
`node_lease.rs`, `node_log_state.rs`, and `node_log_recovery.rs`:

- Admit, queue, reject, or fence a command.
- Start, reconcile, retry, or abandon an immutable publication.
- Release an output after object or fleet durability proof.
- Renew, activate, publish, quiesce, release, take over, or tombstone control.
- Attach and consume one exact recovery overlay.
- Select a due timer, retry, or bounded maintenance action.
- Start or refuse an eviction, pressure drain, or ownership move.

`Control::validate_transition` remains the single-record predicate. The kernel
composes that predicate with node lease, publisher, follower, recovery, and
local lifecycle observations. It must not copy transition rules into a second
simulator-only implementation.

The coordination seam has three value families:

```text
CoordinationState
  = durable observations + local actor state + admitted work watermarks

CoordinationInput
  = request | timer | I/O completion | lease observation | shutdown | fault

CoordinationDecision
  = next local state + ordered effects + externally releasable outputs
```

An effect carries a stable operation ID, complete preconditions, bounded size,
and the authority token it observed. Production adapters execute effects and
feed typed completions back into the kernel. They never mutate kernel state
behind its back. Retried completions are idempotent; unknown completions fail
closed. Only the production adapter owns secrets, byte bodies, ETags, file
handles, and network connections.

### Preserve one production implementation

The async actor becomes an executor around the kernel:

1. Poll one external input or completed effect.
2. Call the pure step function.
3. Persist or dispatch the returned effects in order.
4. Feed every success, explicit rejection, timeout, cancellation, and
   ambiguous result back as a typed input.
5. Release a response only when the returned decision names a satisfied
   durability proof.

Production may coalesce safe reads or immutable uploads, but coalescing cannot
hide a completion from the state machine. Timers carry explicit logical
deadlines. Random identifiers and backoff jitter are supplied as inputs. This
makes the test driver and production loop exercise the same decisions without
making object I/O synchronous or moving large bytes into the kernel.

### Drive a seeded adversarial simulator

Add a test-only simulator whose complete run is determined by a printed seed
and scenario version. It models several nodes, Cells, clients, one linearizable
conditional object store, local disks, follower logs, and independently
advancing clocks. At every step it chooses among enabled inputs and may:

- Delay, duplicate, reorder, reject, or lose the response to an accepted I/O.
- Crash a process or individual async effect at every production await seam.
- Expire, renew, or observe a node lease near its deadline.
- Fill owner, follower, cache, or scratch disk.
- Partition peer traffic independently from object storage.
- Restart with empty mutable state and retained immutable cache.
- Race publish, takeover, recovery attachment, migration, eviction, and drain.
- Pause a handler or stream after acceptance but before completion.

Every failure prints the seed, minimized event trace, initial state, and final
invariant violation. CI keeps a bounded deterministic seed corpus plus every
historical failing seed. A scheduled broad job explores new seeds. A protocol
change may retire a seed only when the scenario is invalidated and the reason
is recorded.

The simulator continuously asserts:

```text
at most one output-capable owner per Cell epoch
acknowledged(commit) => object_covered(commit) OR recoverable_fleet_covered(commit)
serving => live node lease AND matching owner/epoch/root AND no pending overlay
published_root and logical sequence never rewind
takeover cannot consume a partial or unpinned recovery tail
released owner cannot emit output or revive its mutable database
all reservations, leases, and durable dependencies remain bounded and owned
eventually, after faults stop, accepted work resolves or reports unknown
```

Deliberately broken variants disable one fence, durability gate, CAS predicate,
or overlay precondition. The checker must find each defect within a pinned seed
budget. This proves that green properties are capable of observing the class of
failure they claim to prevent.

### Model the smallest durable protocol

Add a TLA+ model under `crates/crab-cell-runtime/model/` for two Cells, up to
three nodes, bounded commits, object publication, follower proof, lease expiry,
takeover, recovery attachment, release, and stale-owner output. Keep SQL,
payload bytes, transport encoding, Blob, Queue, Workflow, Cron, and JavaScript
outside the model; they matter only as accepted work and durable effects.

Each checked configuration pins an expected verdict. Passing configurations
must establish single-writer, no-lost-acknowledgement, monotonic-root, and
fencing invariants. Negative configurations intentionally remove a rule and
must produce a counterexample. The model is a reviewed specification, not a
generated mirror: `model/README.md` records the production commit, modeled
transitions, known abstractions, and every deliberate delta.

The pinned TLC version and checksum live in the verification tooling, not in a
production dependency. A fast small configuration runs for protocol changes;
the broader state space runs in scheduled CI. A model result never replaces
Rust simulation or real-fleet qualification.

### Exit the assurance phase only with evidence

- Production and simulation call the same pure transition functions.
- Every coordination await seam is representable as a simulator completion or
  crash point.
- Historical seeds replay exactly and broken variants fail as expected.
- The TLA+ model and delta ledger name every authority and durability
  transition in the production protocol.
- The simulator covers recovery, rollout, eviction, and pressure movement in
  addition to the happy path.
- Existing async integration tests remain as adapter and real-I/O proof.

## Make warm resident reads local

### Put local lookup before remote discovery

`RepositoryCellRouter` first derives the deterministic `CellTarget`, then asks
`CellRuntime` for an active local route. The actor map already owns the only
valid process-local admission capability and the verified `CatalogProof` used
to create it. Extend that lookup to return an opaque local route certificate
containing the handle and its current incarnation, code, schema, owner session,
epoch, root, and local hydration state.

The certificate is process-local, non-serializable, and short-lived. It is not
an ownership cache. Lookup succeeds only when:

- The runtime node lease is currently usable.
- The actor is `Active`, not activating, quiescing, draining, migrating, or
  fenced.
- The requested target exactly reconstructs the actor's verified catalog
  entry and Cell ID.
- The actor's publisher still holds the matching owner session, epoch, and
  authoritative root.
- The current admission generation is the same generation embedded in the
  returned handle.

On any miss, the router uses the existing slow path: load and verify catalog,
load exact control, route to a live peer or activate from the authoritative
root. Catalog and control observations from that path may populate immutable
process-local acceleration, but they never permit takeover or publication
without the normal fresh CAS preconditions.

### Invalidate from the owner seam

The runtime removes or rejects a local route before it starts migration,
quiescing, pressure drain, explicit drain, or shutdown. A node-lease fence
atomically closes route lookup and Cell admission before peer takeover is
possible. A failed control renewal fences the actor; a later local request
cannot fall back to its stale handle.

No timeout-based cache invalidation is required for local ownership. The
capability lifetime is coupled to actor admission and the node lease. Remote
owner endpoints and cold Cells are never served from this local index.

### Finish bounded background hydration

Sparse activation remains legal and may begin serving after exact-root
verification. It is called `ActiveSparse`, not fully resident. Wire
`Db::hydration` and `hydrate_step` through bounded SQL-worker jobs so an
active sparse Cell progressively resolves inherited pages while foreground
work remains prioritized.

Hydration:

- Reserves incremental local disk before each page batch.
- Uses existing page-I/O, object-I/O, and job admission.
- Verifies every directory node, frame, page checksum, and final hydration
  count.
- Pauses under foreground queue, disk, or object-store pressure.
- Is cancel-safe on fence and eviction; partial verified pages remain only as
  disposable local state.
- Promotes the actor to `ActiveResident` only after every inherited allocated
  page is locally materialized or superseded by a local write.

`ActiveResident` is a performance classification, not durable authority. A
subsequent database growth stays local, while a migration, root change, or
takeover creates a new activation and must earn the classification again.

### Define the zero-operation claim precisely

A **qualified warm resident read** is an authenticated read-only request whose
Cell is `ActiveResident`, whose handler needs no remote Blob or object-store
value, and whose result fits existing response admission. Local SQL and KV
reads remain part of the claim. From the start of Cell
routing through completion of its SQLite query, it performs zero calls to the
fleet object store. TLS, ingress, application authorization, and peer transport
are measured separately.

The claim does not include:

- Cold or sparse activation and first-page faults.
- A mutation, which waits for object or fleet durability proof.
- Explicit application access to remote Blob or object storage.
- Migration, backup, retention, or scheduler scans.
- A request initially received on a non-owner and forwarded to the owner.

Instrument the storage adapter with request origin (`route`, `page_fault`,
`publication`, `primitive`, or `maintenance`). Qualification fails if a warm
resident read increments any origin. Report p50, p95, p99, and maximum for
local-route lookup, actor queue, SQL execution, and full request separately.

### Exit the resident-routing phase only with evidence

- Repeated local lookups perform no catalog-head, catalog-page, control, node
  directory, or immutable-root object reads.
- A qualified warm resident read records zero object-store calls end to end.
- Concurrent fence, drain, migration, and takeover tests never use an old
  admission generation.
- Sparse reads remain exact while hydration proceeds, and promotion occurs
  only after complete verified local coverage.
- Hydration and route acceleration stay within the node memory, disk,
  descriptor, and job envelope.
- Crab latency is reported from matched hardware; the Celld fixed-host figures
  remain an external baseline until reproduced under the same workload.

## Balance ownership under live pressure

### Separate placement from authority

Add a private fleet placement controller in `crab-http-server` and a pure
planner in `crab-cell-runtime`. The planner consumes a signed, revision-pinned
fleet observation and produces advisory actions. It cannot write Cell control,
construct an owner, or bypass actor admission.

```text
signed node observations + owned Cell summaries
  -> pure weighted planner
    -> keep | stop-acquiring | hydrate | evict | quiesce-and-release
      -> actor proves quiescence and durability
        -> CellAuthority release CAS
          -> ordinary preferred-node activation
```

The existing scheduler rendezvous continues to assign catalog maintenance
scans. Ownership placement is a different decision and must not overload that
interface or make scheduler progress an authority prerequisite.

### Advertise live usable capacity

Extend the signed node advertisement with a versioned placement block derived
from the same resource envelope used by admission. It includes:

- Effective memory limit, current cgroup working set or process RSS fallback,
  allocator estimate, and memory headroom.
- Effective disk limit, physical free bytes, admitted local/follower/scratch
  bytes, and disk headroom.
- Available file descriptors, worker/job credits, activation backlog, and
  publication/follower backlog.
- Active, sparse, resident, quiescing, and owned Cell counts.
- Draining and pressure tier, placement weight, sample generation, and sample
  time.

On Linux, the resource probe reads cgroup v2 limits, current charge, and memory
events. On other platforms or unreadable cgroups it uses the already-qualified
process and host probes and marks the source. The controller reasons about
usable headroom, never host totals hidden behind a container limit.

Advertisements remain short-lived and signed. A mixed fleet that does not
publish the required placement version may route existing ownership normally
but performs no proactive movement. This is a rollout gate, not a compatibility
fallback.

The current signed placement schema is version 2. It carries three bounded
backlog counters: publication pressure in 1 MiB units of retained native work
and unrooted node-log bytes, plus admitted hydration and primitive job counts.
An advertisement without a runtime measurement has no signed placement block.
Draining nodes retain a signed block with zero free capacity so donors remain
visible but cannot receive new Cells. The pure transfer planner caps one
tick at two Cells and 8 GiB of projected disk restore, with absolute receiver
memory, disk, Cell-slot, and job-credit checks. It requires two stable samples
and a 60-second residence/cooldown for ordinary movement; explicit drain and
sustained shedding bypass the score-gain gate only.

Ownership movement has two ordinary reasons, and both stay advisory. A
balancing move answers a count question: each member's weight is its declared
Cell capacity, its target is the fleet's owned Cells shared by that weight
(rounded up, so the targets always cover the fleet), and only the member with
the most Cells per unit of weight may donate. That donor releases at most its
surplus, at most the two-Cell batch, and at most the receivers' room below a
two-percent deadband. Receivers are members below their own target that are
fresh, eligible, and not shedding, least dense first. One snapshot therefore
elects one donor and cannot hand a Cell to a member that its own next sample
would send back. A relief move answers a resource question and keeps the
material headroom-gain gate. Both paths share settlement, residence, cooldown,
and projected receiver capacity, so a balancing move cannot skip an actor
gate.

A balancing view fails closed. Every live member must publish a fresh signed
placement block, and every sample must be taken after the instant this node
dispatched its previous movement batch: a partial or mixed total lowers every
target and moves Cells that come straight back. Without that complete view the
loop moves nothing on the count rule and keeps the drain and relief paths,
which carry their own per-node freshness checks. These are advisory limits;
the actor still rechecks the exact Cell generation and a transfer-specific,
indexed durable-work inspection before release. Retained request/inbox
outcomes, Blob metadata, Queue producer identities, and future Cron schedules
may follow the exact root. Live or due source effects, ready or leased Queue
messages, pending Workflow activities/timers, due Cron delivery, and unknown
inspection state block movement; the maintenance-release inventory remains
conservative and unchanged.
The private server controller runs every 15 seconds, samples signed live nodes
and actor-approved local candidates, then releases exact generations through
the actor before sending an authenticated receiver activation hint. If receiver
activation fails, the exact unowned root remains available for normal routing.
Each tick reports confirmed source releases and successful receiver activations
separately; a started drain is not counted as a completed move.
The scale-down host state stops new acquisition, paces exact actor releases,
and reports released, blocked, and remaining Cell counts while retaining the
node lease and facilities for incomplete deadlines. A successful drain invokes
the existing terminal shutdown only after ownership reaches zero. Measured
per-Cell disk demand, shared fleet-wide movement accounting, and protected
provider/Kubernetes evidence remain qualification work before production
rollout.
The mTLS management listener exposes `POST /internal/cells/v1/scale-down` for
an operator or orchestrator to request this same drain: `200` means the node
reached `Stopped`, while `202` reports a bounded incomplete drain that is safe
to retry without releasing blocked ownership.

### Weight Cells by measured cost

Each actor emits a bounded placement summary with exponentially weighted
recent demand:

- Reserved SQLite/native memory and local disk bytes.
- Foreground CPU time and request rate.
- Queue depth and oldest accepted-work age.
- Publication and follower-proof backlog.
- Sparse bytes remaining and estimated restore cost.
- Last foreground use and next durable scheduler deadline.

The first implementation uses existing reservations as hard cost and recent
CPU/request observations only as tie-breakers. It does not guess unmeasured
memory. Later weights require receipt-backed evidence that they improve tail
latency or convergence. Missing or stale summaries use conservative declared
reservations.

Weighted rendezvous ranks eligible nodes for a Cell using the fleet snapshot
digest, Cell ID, node session, and normalized resource headroom. The ranking is
stable for one snapshot and changes minimally when membership changes. A node
is ineligible when draining, hard-pressured, lease-stale, release-incompatible,
over its activation backlog, or unable to reserve the Cell's conservative
cost.

### Add hysteresis and bounded movement

The planner uses three pressure states derived from existing reserves rather
than new environment switches:

| State | Entry | Exit | Action |
| --- | --- | --- | --- |
| Normal | All declared headroom above soft reserve | N/A | Admit preferred cold Cells and retain active Cells. |
| Soft pressure | Any resource crosses its soft reserve for multiple samples | All resources clear a higher exit margin for multiple samples | Stop new acquisition, pause background hydration, evict oldest eligible idle Cells, and plan bounded quiescent releases. |
| Hard pressure | Memory, disk, descriptors, or job backlog crosses its hard reserve, or cgroup events show sustained reclaim/OOM risk | Return through soft pressure; never jump directly to Normal | Reject new local activation, shed eligible Cells in paced batches, preserve durability work, and fail readiness if safe progress cannot restore reserve. |

Entry and exit use separate thresholds, consecutive-sample requirements, a
minimum Cell residence time, a post-move cooldown, and a per-node movement
budget. Only one bounded donor set moves for a snapshot generation. The fleet
caps concurrent activation, hydration, and drain bytes as well as Cell count,
so many large Cells cannot evade a count-only limit.

Idle eviction first closes SQLite and retains verified immutable cache within
budget. A balancing or pressure move additionally releases ownership to
`Idle`. Normal cache eviction may keep an active owner only if the local actor
can safely reopen within its existing authority; this design initially avoids
that additional hibernation state and uses full release.

### Move through ordinary recovery

Crab does not add a direct owner-transfer record. A movement is:

1. Planner proposes a destination and records the fleet snapshot digest.
2. Current actor rechecks eligibility and enters `Quiescing`.
3. It closes new admission, drains accepted work, and makes every acknowledged
   commit object-covered or recovery-pinned.
4. It CASes the exact owner, epoch, and root to unowned `Idle`.
5. Router preference sends the next activation to the highest-ranked eligible
   node, which rereads control and acquires through the existing idle-
   acquisition path.
6. The destination restores the exact immutable root and only then serves.

The proposed destination is a hint. If it disappears or loses capacity before
step five, the next eligible node may acquire. The donor never releases merely
because a receiver promised capacity. Failure before release leaves the donor
authoritative; failure after release leaves an exact unowned root that any
eligible node can acquire. Hot live migration and writable local-file transfer
remain outside scope.

Node shutdown uses the same mechanism with a stricter paced-drain budget. It
stops acquisition first, drains quiescent Cells in bounded batches, and reports
remaining active, durability-blocked, and restore-in-flight counts. A deadline
may terminate availability, but it cannot skip the durability or authority
preconditions for a clean handoff.

### Exit the balancing phase only with evidence

- Adding and removing nodes converges weighted resource load without changing
  a Cell's acknowledged contents or producing two output-capable owners.
- A skewed mix of tiny, memory-heavy, disk-heavy, hot, sparse, Queue, and
  Workflow Cells balances by measured cost better than count-only placement.
- Stale, missing, mixed-version, or forged samples stop proactive movement and
  do not stop ordinary authority routing.
- Hysteresis and cooldown prevent ping-pong under oscillating cgroup memory,
  disk, descriptor, and workload pressure.
- Receiver death before and after release recovers through the ordinary exact-
  root path.
- Movement rate, concurrent restore bytes, foreground p99 impact, and time to
  restore reserve remain within the signed profile thresholds.
- Fleet drain with one failed receiver remains bounded, observable, and safe.

## Apply the architecture uniformly to native primitives

SQL, KV, Blob, Queue, Workflow, Cron, and effects remain behaviors behind one
Cell interface. None receives a separate owner, publisher, scheduler, or
replication protocol.

| Primitive | Resident and movement rule |
| --- | --- |
| SQL and KV | Read-only operations participate in the qualified resident-read claim when their complete SQLite state is local. Mutations use the same output and durability gates. |
| Blob and object storage | Blob metadata remains Cell state, while explicit remote body access is reported as `primitive` object I/O and is outside the zero-object-operation read claim. Blob references needed by an acknowledged result remain retention dependencies during drain. |
| Queue | Ready messages, leases, dead-letter work, and oldest due age contribute to eviction eligibility and placement cost. A live delivery lease prevents movement. |
| Workflow | Pending activities, timers, retries, and terminal cleanup contribute to eviction eligibility and placement cost. Activity completion still enters through the normal command and durability path. |
| Cron | The next durable fire time remains in authoritative Cell control. A move must leave it visible to scheduler scans; no process-local timer may be the only record. |
| Effects | Pending delivery and resolution prevent unsafe eviction. The coordination kernel models their durable acceptance and completion without understanding application payloads. |

Primitive transition logic remains deterministic and separately testable. The
coordination kernel models it as accepted work, durable state change, due work,
and an optional external effect. This keeps the kernel deep without making its
interface grow for every primitive operation. The placement planner consumes
bounded summaries, not Queue messages, Workflow histories, Blob bodies, or SQL
rows.

Qualification includes mixed primitive Cells so a Queue backlog, long-running
Workflow, imminent Cron fire, or large Blob dependency cannot be hidden by an
otherwise idle request rate. A Cell that cannot safely quiesce stays owned and
reports the blocking class; pressure policy may reject new work but never
silently drops the primitive's durable obligation.

## Stream canonical Cell publication

### Describe the current allocation

Native capture preparation copies each admitted segment into an owned scratch
file and reopens it through the bounded authenticated inspector. Bundle
preparation now has the same source shape: `Bundle::decode_file` and recovery
manifest reopen retain a verified file path plus row metadata, while selected
rows are read by exact extent and uploaded through a replayable multipart
source. The bundle is first written to a deterministic digest-scoped staging
key and promoted through the content-addressed CAS before the staging key is
removed.

Admission bounds the total bytes, but admission is not the same as bounded
resident memory. The remote recovery path no longer retains a complete bundle
body: it streams the provider response to a runtime-owned session/cell scratch
file, checks the outer digest before CRB1 parsing, reserves the exact remote
size in the shared recovery `DiskBudget`, and performs structural/LTX
verification on a blocking worker. The reservation travels with the returned
overlay until its temporary file is dropped. The node restart inventory counts
regular files left in stale session directories—including compaction and
recovery scratch—before admitting new work; it rejects symlinked or special
entries. Server startup warns with the stale-session count, charged bytes,
remaining shared disk budget, and budget capacity when earlier session
directories remain. That reservation is conservative accounting, not cleanup;
the files stay charged until an exclusive reclaim protocol is proved. Newly
encoded node-log overlays still begin in memory and remain a
separate peak-residency qualification item. A 5 GiB Cell is built from bounded
cuts; its size must not increase the memory used by any later incremental
append.

### Replace body ownership with admitted sources

The private append implementation will consume an ordered list of admitted
sources rather than `Vec<u8>` bodies. A source is one of:

- An exact captured local segment path plus its immutable `SegmentInfo`.
- An exact byte range within an already verified recovery bundle.

The source list owns routing and expected metadata, not decoded contents. It is
private to `crab-ltx`; callers continue to use `CellReplica::prepare`,
`prepare_bundle`, and `prepare_recovered_overlay`.

For every source, preparation performs this sequence:

1. Reserve dirty and scratch capacity for the complete operation.
2. Open the exact source through the injected `Host` filesystem.
3. Stream LTX verification through bounded reads.
4. Stream the authenticated page index into a synced scratch file.
5. Verify the complete body BLAKE3, metadata, page order, TXID range, database
   checksum, and index digest.
6. Upload the body and index through bounded multipart reads.
7. Retain only the validated descriptor and the scratch reference needed by
   directory construction.
8. Feed authenticated index entries into initial or incremental directory
   construction.
9. Remove owned scratch after success or failure.

Preparation still returns one `PreparedRoot`. Uploading immutable bytes is not
publication. A cancellation, failed upload, or caller drop may leave orphan
immutable objects, but it cannot create a publishable root missing a dependency
or change Cell control.

### Keep verification exact

Streaming must preserve the current verification contract:

- Every segment matches its captured `SegmentInfo`.
- TXID ranges are contiguous and begin at the expected predecessor.
- Page numbers are strictly ordered and exclude the SQLite lock page.
- Every frame hash and decoded page checksum matches.
- The resulting database checksum equals the declared endpoint.
- A bundled segment remains bound to its bundle digest and exact extent.
- An index digest binds the exact encoded index bytes.
- The root binds Cell, incarnation, sequence, schema, TXID, checksum, directory,
  and segment pages.

No error may cause a retry to reinterpret bytes through a less strict reader.

The file-backed bundle contract is deliberately fail-closed. A provider read,
digest, footer, row, LTX, or multipart error is returned to the caller; it does
not fall back to object listing, a native row, or an alternate bundle source.
Temporary files are owned by the bundle and are removed when the overlay is
dropped or decoding fails. Remote staging keys are deterministic for the
content digest, so a retry or failover converges on one unreferenced upload
target rather than creating a fresh key for every attempt. A process death can
still leave that one private staging key; remote staging scavenging remains a
provider-retention qualification and is never used as a recovery reader.
### Bound transfers

The implementation uses existing configured facilities rather than new
environment options:

- Bounded filesystem transfers for verification and scratch.
- Existing object I/O permits.
- Existing CPU, dirty-job, recovery, and scratch admission.
- Existing multipart object uploads.
- At most one bounded body window and one bounded index window active per
  worker.

The exact buffer sizes remain implementation constants selected from existing
limits. They are recorded in qualification receipts and changed only with
before/after measurements.

### Test every ambiguous point

Fault injection covers:

- Source read failure before and after verified bytes.
- Scratch create, write, sync, rename, and parent-sync failure.
- Index encoding or checksum failure.
- Multipart failure before and after an accepted part.
- Cancellation while verification or upload is running.
- Local disk exhaustion before admission and after an installed scratch file.
- Object-store timeout and accepted-write response loss.
- Retry with already-uploaded immutable objects.

The observable assertions are unchanged root authority, bounded retained
admission, cleaned owned scratch, and exact retry behavior.

### Exit the publication phase only with evidence

The phase is complete when:

- A 5 GiB incompressible Cell grown through legal bounded cuts restores and
  compacts exactly under declared peak RSS and scratch ceilings.
- The maximum legal captured append uses memory proportional to configured
  transfer buffers rather than its complete body and index size.
- No remote body read or upload begins before complete operation admission.
- Corrupt body, index, bundle, or directory bytes fail closed.
- Cancellation and disk-full tests leak neither capacity nor owned scratch.
- Existing exact-root, compaction, follower recovery, and source-loss suites
  remain green.

## Persist only verified directory acceleration

### Keep authority out of the cache

The authenticated radix directory is the canonical metadata structure. Its
incremental publisher already changes only affected leaves and ancestors, and
initial construction streams completed leaves. The remaining scale concern is
repeated remote metadata reads after the process-wide 8 MiB memory cache turns
over.

The server may supply `crab-ltx::Host` with a caller-owned local directory-node
cache. The default host remains memory-only. These are two real adapters at the
existing host seam: ephemeral library use and server-managed persistent cache.

A cache key binds:

```text
backing store identity
+ Cell identity
+ incarnation
+ immutable object path
+ BLAKE3 digest
```

Cache installation uses an exclusive scratch file, file sync, atomic rename,
and parent-directory sync. Every hit is rehashed before decoding. A corrupt,
short, or missing entry is deleted or ignored and rebuilt from the authoritative
origin. It never causes root rollback or fallback to another format.

The persistent cache is byte-bounded and evictable. Eviction changes latency,
not correctness. Backup and retention traversal continues to use uncached
origin reads where it must prove that remote dependencies still exist.

### Measure before sharding locks

The current in-memory cache uses one process-global mutex. The implementation
first adds contention telemetry and the 1,000/10,000-Cell workload. It shards
or replaces that lock only when measurements show material wait time. This
avoids adding a speculative concurrency structure.

### Exit the metadata phase only with evidence

- 1,000 and then 10,000 genuinely open Cells have bounded metadata RSS.
- Persistent cache disk use stays within its admitted share.
- Cold, memory-cache, and disk-cache lookup latency are reported separately.
- Cache corruption and eviction cannot change restored bytes.
- Remote reachability checks do not accept local presence as proof.

## Make resident lifecycle explicit

### Separate local residency from distributed control

Cell control continues to use `Recovering`, `Serving`, `Idle`, and `Tombstoned`.
The runtime additionally tracks a local, non-serialized lifecycle for active
handles:

```text
Cold -> Activating -> ActiveSparse -> ActiveResident
                     |       |             |
                     +-------+-> Quiescing -> Cold
                     |       |             |
                     +-------+-> Fenced <---+
```

This local lifecycle does not create a second authority record. `Active` is
the common admission state represented by `ActiveSparse` and `ActiveResident`.
Either is valid only while authoritative Cell control still names the process
session and epoch. The distinction is performance-only: both use the same
SQLite writer, authority, root, and durability gates.

### Let the actor own eviction eligibility

The actor has the information required to decide whether a Cell is quiescent.
Router timers or an external LRU cannot independently prove safety.

A Cell is not evictable while it has any of:

- Accepted commands or queries.
- Tentative SQLite state.
- Pending object publication or ambiguous control transition.
- Fleet-covered data not yet object-covered or recovery-pinned.
- Live Workflow activity, Queue lease, effect delivery, or state stream.
- Migration or maintenance work.
- A scheduler deadline that cannot be transferred safely.

When active admission is exhausted, the runtime selects the least-recently-used
eligible actor. No new configuration option is added. If no actor is eligible,
the request receives bounded capacity failure rather than forcing unsafe
eviction.

### Drain in one order

Eviction performs:

1. Close new local admission.
2. Wait for accepted work and output gates.
3. Reconcile or publish every pending root.
4. Ensure follower-covered tails are object-covered or recovery-pinned.
5. Stop Cell-local activities and state streams through their existing
   cancellation contracts.
6. Close SQLite and release its reservations.
7. CAS authoritative control to `Idle` only if this exact owner, epoch, and root
   still win.
8. Remove mutable files for this activation.
9. Retain only independently verified immutable cache entries within budget.

Failure before step seven leaves authority owned and the actor fenced or
recoverable. Failure after step seven cannot revive the closed local writer.

### Define warm activation narrowly

Warm activation means that verified immutable directory nodes and pages may be
available locally. It still acquires authority, opens the exact control-pinned
root, and creates a fresh sparse writable SQLite session.

Do not call cache-warm activation a resident request. Cache warmth can reduce
activation I/O but cannot prove that every SQLite page needed by a request is
local. Only `ActiveResident` supports the zero-object-operation read claim.

Reopening an old mutable SQLite session is outside this design. It would need a
separate crash-safe claim, clean-close marker, exact-root binding, and filesystem
qualification. Implement it only if the measured fresh sparse path remains a
material bottleneck after immutable caching.

### Exit the lifecycle phase only with evidence

- Sustained churn beyond active capacity produces bounded eviction and
  activation rather than permanent exhaustion.
- Every acknowledged outcome survives eviction and reactivation.
- Ineligible actors are never selected under pressure.
- Fencing during every drain step prevents further output.
- Hot, immutable-cache-warm, and cold activation have separate measurements.
- Process restart treats ambiguous mutable files as quarantine, not authority.

## Use one resource envelope

The runtime already shares many admission facilities. Qualification must prove
that the total model covers every local consumer without omission or double
counting.

| Consumer | Required accounting |
| --- | --- |
| SQLite main, WAL, and SHM | Active Cell disk and descriptors; the runtime ledger reserves eight descriptors per active Cell and exports used/capacity gauges |
| Retained captured LTX and checksum sidecar | Managed session disk |
| Sparse materialized pages | Incremental Cell disk |
| Directory-node and immutable-page cache | Evictable cache disk |
| Follower node-log tails | Follower disk budget |
| Verification, restore, and compaction scratch | Full-job scratch budget |
| Git, LFS, archive, and Release staging | Shared product local disk |
| SQLite page caches and actor state | Active Cell memory |
| Codec, dirty, recovery, and activity jobs | Shared job and memory credits |
| Background Cell hydration | Incremental disk, page I/O, object I/O, and job credits |
| Coordination simulator traces | Test-only bounded memory and artifact retention |

Before a large operation reads remote bodies, it reserves its complete estimate
and remeasures physical free space against the node reserve. A failed operation
retains conservative accounting until installed files are reconciled or the
owning handle is discarded.

The work adds no environment variable. Resource-derived server configuration
and `cells capacity --json --live` remain the operator interface.

This phase also qualifies retention at scale:

- Backup pins and in-flight backup advertisements race collection safely.
- Collection streams large inventories and bounds deletion batches.
- Current controls, retained releases, and every pin remain exact roots.
- Retired follower lanes disappear only after authority covers their epochs.
- Cross-provider export is implemented before it is advertised as a recovery
  contract.

## Qualify production behavior

### Define claims before running load

Every qualification run declares:

- Source commit, release tag, image digest, and chart digest.
- Provider and object-store version/topology.
- Node CPU, memory, effective local disk, filesystem, and file-descriptor limit.
- SQLite page size and database size distribution.
- Number of genuinely open Cells.
- Transaction body size and pages changed per transaction.
- Read/write mix and hot-key distribution.
- Follower count and durability policy.
- Object-store latency and injected failure schedule.
- Target throughput and latency thresholds.

Profile names do not imply a result. Small, medium, and large profiles receive
separate receipts. The 10,000-Cell or 1,000-mutation/s result is claimed only on
profiles that actually achieve it.

### Run the capacity matrix

| Stage | Workload | Required evidence |
| --- | --- | --- |
| Large database | 100 MiB and 5 GiB; compressible and incompressible | Exact source-loss recovery; bounded peak RSS and scratch |
| Residency | 1,000, 5,000, then 10,000 open Cells | Bounded RSS, threads, descriptors, cache, and SSD |
| Resident reads | Repeated read-only requests to fully hydrated local owners | Zero object-store calls; route, queue, SQL, and end-to-end latency percentiles |
| Aggregate writes | 1,000 mutations/s across eight or more Cells | At least 95% success, latency percentiles, exact replay |
| Hot Cell | One Cell under bounded concurrent writes | Queue bounds, logical/published-head lag, no starvation |
| Maintenance overlap | Hydration, compaction, backup, retention, scheduler | Foreground latency, bounded queues, no root rewind |
| Lifecycle churn | Working set exceeds active admission | Bounded eviction, activation, and authority movement |
| Fleet convergence | Add/remove nodes with uneven Cell costs and shifting hot sets | Weighted balance, bounded moves, no oscillation, declared p99 impact |
| Resource pressure | Oscillating cgroup memory, disk, descriptor, and job pressure | Hysteretic shedding, reserve recovery, no unsafe release |
| Takeover | Cold and immutable-cache-warm successors | Exact root, monotonic epoch and sequence, takeover time |

### Inject the fault matrix

- Kill the owner before and after SQLite commit.
- Kill after follower proof but before object publication.
- Lose an accepted control-CAS response.
- Delay or throttle immutable uploads.
- Partition peer management traffic without partitioning object storage.
- Expire the owner advertisement and reconnect the stale process later.
- Fill owner, follower, and scratch disk independently.
- Lose follower ACKs and restart followers simultaneously.
- Kill recovery after each overlay attachment and pinning step.
- Restart with empty owner-local SQLite and LTX directories.
- Roll between compatible releases while work continues.
- Enter maintenance with retained incompatible runtime work.
- Return stale or mixed-version placement samples during a rebalance.
- Kill a planned receiver before release, after release, and during restore.
- Oscillate cgroup memory around both pressure thresholds.
- Replay every historical simulator seed and crash each modeled await seam.

After every fault, verify one authoritative owner, no stale-owner output,
monotonic sequence and root, stable replay, no lost acknowledged result, and no
leaked lease or reservation.

### Retain these measurements

- Process RSS and allocator peak.
- Descriptors and threads.
- Main, WAL, LTX, sparse, cache, follower, and scratch bytes.
- Object-store requests and bytes per command.
- Object-store requests by origin: route, page fault, publication, primitive,
  and maintenance.
- WAL and LTX write amplification.
- Publication and follower backlog.
- Command p50, p95, and p99 latency.
- Local-route hit rate and lookup latency.
- Sparse hydration remaining bytes, throughput, pauses, and promotion time.
- Scheduler pass duration and due lag.
- Placement-sample age, planned and completed moves, rejected moves, pressure
  tier, convergence error, and cooldown suppressions.
- Cold and warm takeover duration.
- Compaction throughput and peak scratch.
- Graceful drain and shutdown duration.

Qualification fails on any correctness violation. It also fails if the process
swaps, exceeds its descriptor reserve, admits beyond disk capacity, allows a
scheduler pass above five seconds, or records less than 95% successful responses
at the configured 1,000-mutation/s target.

Signed receipts bind the measurements to the exact source, image, Pod UID,
provider, profile, and completion time. Local unit tests and in-memory object
stores cannot substitute for these receipts.

### Compare Crab and Celld fairly

A comparative benchmark uses identical:

- Hardware and filesystem.
- SQLite version, page size, and database distribution.
- Object-store implementation, placement, and injected latency.
- Mutation payload, pages changed, and read/write mix.
- Follower count and acknowledgement policy.
- Warmup, duration, and failure schedule.

Exclude V8 and JavaScript handler execution from both measurements. Report
throughput, latency, resource use, recovery time, and write amplification.
Crab may claim an advantage only for a metric demonstrated under the matched
configuration; architectural expectations are not benchmark results.

Compare architecture as well as headline throughput:

- Run the same resident-read instrumentation and report bucket calls, sparse
  page faults, and routing cost separately.
- Run the same seeded fault classes where both protocols expose the seam, and
  publish non-equivalent assumptions rather than normalizing them away.
- Compare count-weighted placement with Crab's reservation- and demand-weighted
  placement on heterogeneous Cells.
- Measure convergence, movement amplification, and foreground p99 during node
  addition, node loss, cgroup pressure, and graceful drain.
- Record model and simulator coverage as assurance evidence, not as a runtime
  performance score.

## Standalone replication contract: hard removal recorded and executed

Commit `4d097cce362` introduced the standalone replication interface and is
reachable from tag `v1.2.4`. That tag exported the epoch-head, standalone paging,
and standalone scheduling APIs. The repository maintainer explicitly authorized
hard removal on 2026-09-18 for the current unreleased breaking change (or the
next breaking release if this branch is cut into a release). The full evidence
record is [the standalone replication audit](standalone-replication-audit.md).

The crate remains `publish = false`, but tagged source, docs, and examples were
treated as potentially shipped. The audit found no workspace production caller;
unknown external usage is handled by an explicit offline migration boundary,
not by keeping a parallel runtime path.

### Preserve proof before removal

Before physical deletion, move still-relevant evidence to the canonical path:

| Standalone evidence | Canonical owner after migration |
| --- | --- |
| Mutable-head CAS race and lost response | `CellAuthority` and `CellPublisher` |
| Exact source-loss restore | `CellReplica` and actor takeover tests |
| Bundle corruption and no fallback | Recovery overlay and node-log recovery tests |
| Sparse partial writes and delayed faults | `CellWritableDatabase` and Cell VFS tests |
| Destination admission before I/O | `CellReplica` restore/compaction tests |
| Range-compaction byte bounds | Cell exact-root compaction tests |
| Provider worker lifetime | Shared `Host` and Cell paged-I/O tests |
| RustFS round trip | Runtime and server source-loss qualification |

Index encoding, decoding, frame verification, and index validation remain in the
private authenticated-index module used by `CellReplica`. The public standalone
facade and its page-map implementation are gone.

### Delete only standalone ownership

The authorized hard-removal change deletes:

- `Replica` and `ReplicaHead`.
- Standalone epoch-head layout and publication.
- `CompactionSchedule` for standalone heads.
- Standalone read-only `PagedDatabase` and `PagedConnection`.
- Standalone-only `Db` helpers.
- Standalone examples, documentation, and tests after evidence migration.
- The standalone branch of `paged_io`; the retained bridge has one Cell variant.

Retain:

- `Db`, WAL capture, snapshots, and checkpoints.
- LTX codecs, checksums, and exact local recovery.
- `Host`, filesystem, executor, disk, dirty, recovery, and scratch admission.
- `bundle::Bundle` and node-log recovery use.
- Private authenticated-index and frame utilities.
- Cell sparse VFS, hydration, roots, directory, compaction, and recovery overlay.

The existing `replica` Cargo feature also enables required Cell remote
mechanics, so its name remains. No feature alias is added.

Old standalone object prefixes are never reinterpreted as Cell roots. If stored
standalone data needs migration, an operator must use an explicit offline
export/import tool with exact-root verification; this change does not delete
remote data and adds no production fallback reader.

## Deliver in dependency order

Each change is independently reviewable and leaves one canonical path.

| Slice | Work | Depends on | Exit evidence |
| --- | --- | --- | --- |
| 1 | Correct ownership docs and add server dependency guard | None | Architecture check rejects production `crab_ltx` imports |
| 2 | Extract the pure coordination kernel without changing effects | 1 | Production adapter parity tests preserve current behavior |
| 3 | Add seeded simulation, broken variants, and trace replay | 2 | Historical seeds replay; each broken variant is detected |
| 4 | Add the small-state TLA+ model and delta ledger | 2 | Positive and negative verdict matrix is pinned |
| 5 | Add actor-owned resident lookup before remote metadata | 2 | Local route has zero catalog/control object reads and fences exactly |
| 6 | Wire bounded background hydration and resident promotion | 5 | Qualified resident reads perform zero object-store calls |
| 7 | Stream native captured-segment verification and upload | 2 | Large native append has bounded RSS and exact recovery |
| 8 | Stream bundle ranges and remove complete-body copies | 7 | Implemented file-backed reopen, exact row reads, staged CAS upload, and recovery tests; 5 GiB RSS/low-disk qualification remains |
| 9 | Persist verified directory-node acceleration | 7 | Cache corruption/eviction tests and bounded residency |
| 10 | Add actor-owned quiescing, idle eviction, and full resource summaries | 5, 6, 9 | Churn test preserves every acknowledged root |
| 11 | Add signed live placement observations and the pure weighted planner | 2, 10 | Mixed-version gate and deterministic plan tests pass |
| 12 | Add hysteretic pressure shedding and paced release/activation | 11 | Pressure and membership tests converge without unsafe movement |
| 13 | Reconcile node-wide disk and job accounting | 7, 10, 12 | Capacity report matches measured local consumers |
| 14 | Complete simulator, fault, capacity, latency, and balancing receipts | 3 through 13 | Signed provider/profile matrix passes |
| 15 | Record standalone support decision | 14 | Named HARD REMOVE decision and offline migration limit |
| 16 | Port unique standalone evidence | 15 | Canonical tests cover every retained invariant |
| 17 | Remove standalone module | 16 | Public surface and documentation match the decision |

The streaming work and standalone deletion are now separate reviewable commits;
canonical Cell tests own the retained invariants, and no standalone test owner
or compatibility facade remains.

Slice 1 implementation evidence: `make architecture-check` now includes an
explicit Cell composition guard. It verifies that `crab-http-server` keeps
`crab-ltx` dev-only and rejects direct `crab_ltx::` use in production source,
while admitting `#[cfg(test)]` modules and dedicated `tests.rs` fixtures. The
guard's temporary-tree regressions live in
`crab/scripts/test_check_architecture_gates.py` under
`CellRuntimeBoundaryTests`; the same gate rejects the retired standalone LTX
epoch-head symbols while admitting Cell-scoped names. Existing actor/publication tests already cover the
required acknowledgement, fence, shutdown, and lost-CAS characterization
cases; no duplicate runtime tests were added. Local Cargo qualification now
passes on the required external workspace target volume; provider, Kubernetes,
and multi-GiB evidence remains pending.

Slices 2–6 now have reviewable seams: `coordination.rs` owns the volatile
admission/fence/publication/migration/shutdown decisions; the actor has one
schedule adapter that passes queue/publisher, lease, and publication-pressure
observations and the kernel centrally decides dispatch, wait, deactivation, or
fence, so `actor.rs` does not duplicate those predicates. Work, migration,
publication, renewal, and stale-hydration completions also carry their fence
observation into one kernel transition; the actor only maps the returned `Fence`
decision to cleanup, while pending commands remain busy until proof. The
schedule transition also distinguishes `ReadyToDeactivateFenced` from a normal
live drain, so release-path selection does not re-read actor lifecycle state;
background hydration and renewal use the same queue/publication/lease
observation boundary; the test-only
simulator replays fixed seeds and checks acknowledgement/publication invariants;
the pinned TLC runner has positive and deliberately broken configurations; and
`CellRuntime::resident_handle` is attempted before catalog/control reads while
bounded worker hydration promotes only verified sparse roots. Slices 10–12 also
have pure resource, eviction, placement, pressure, and movement-budget
contracts, with the runtime/SQL/hydration/primitive-job ledger, stale-session
restart inventory, actor-owned movement seam, and bounded transport-codec
admission wired; complete advertised/metric parity, cold-placement execution,
    and multi-process qualification remain open. The version-4 receipt matrix
    is versioned and size-bounded, with signing, profile/threshold binding,
    artifact binding, fault/artifact/ownership evidence, runner emission, and
    exact source/image release binding implemented.

## Verify every slice

Use one worktree-specific external Cargo target directory.

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-ltx --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-ltx --features replica --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-cell-runtime --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-http-server --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-main \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings

node crates/crab-cell-runtime/docs/validate.mjs
```

After slices 3 and 4, the same verification entry point also runs the pinned
simulation corpus and the fast TLC configuration. The model directory exposes
one checksum-verifying script so local and CI runs use the same TLC build. The
scheduled broad simulator and model jobs archive seeds, traces, counterexamples,
and the source revision; they are not hidden behind a retry-until-green loop.

Compilation and unit tests are necessary but not production qualification. Run
the real RustFS, Compose, Kubernetes, provider, and signed-receipt gates described
in [delivery.md](delivery.md) before changing readiness claims.

## Reject these shortcuts

- Raising `Limits` without removing resident allocations.
- Treating catalog entries as active Cells in capacity reports.
- Using local cache presence as remote-retention proof.
- Reopening an ambiguous local database instead of the authoritative root.
- Letting eviction policy inspect less state than the actor.
- Calling a sparse or cache-warm activation fully resident.
- Claiming zero bucket operations while excluding router or page-fault calls
  from instrumentation.
- Caching catalog or control as a substitute for the node lease, actor
  admission capability, or authority CAS.
- Copying production transition rules into a simulator-only state machine.
- Accepting a model that cannot detect deliberately broken protocol variants.
- Moving hot writers or releasing ownership before actor quiescence and
  durability proof.
- Balancing only by Cell count when declared reservations differ materially.
- Reacting to one pressure sample without a deadband, cooldown, or movement
  budget.
- Adding another configuration mode for standalone versus Cell publication.
- Keeping aliases or dual readers after an approved hard cut.
- Deleting standalone tests before moving their unique proof.
- Comparing Crab and Celld with different durability or object-store conditions.

## Declare completion precisely

The canonical scaling design is complete only when:

1. Production and seeded simulation use the same coordination kernel, broken
   variants fail, and the TLA+ verdict matrix covers the declared protocol.
2. Publication, metadata, hydration, activation, and eviction satisfy their
   bounded tests.
3. A qualified warm resident read performs zero object-store operations, and
   its matched latency receipt reports every layer of the request.
4. Weighted placement, pressure shedding, and paced drains converge without
   unsafe ownership movement or foreground latency beyond the declared bound.
5. Every acknowledged result survives the complete owner and disk-loss matrix.
6. Signed receipts establish the claimed Cell count, throughput, latency, and
   resource envelope for each advertised profile and provider.
7. Backup, retention, follower retirement, release rollout, and shutdown pass
   while load continues.
8. The standalone contract has a recorded HARD REMOVE decision and offline
   migration limit.
9. The removal retains shared mechanics and canonical proof without an
   epoch-head fallback.
10. Documentation describes measured results rather than target capacity.

Until those conditions hold, describe Crab Cell Runtime as functionally complete
but not production-qualified at the target scale.
