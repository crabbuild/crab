# Plan 025: Close the production host and protected qualification gates

> **Executor instructions**: This is the executable follow-up for the audited
> `crab-cell-runtime` production-readiness request. Work from the current
> source, not from aspirational documentation. Keep one canonical production
> owner, reuse the existing application/host/qualification contracts, and do
> not manufacture provider, Kubernetes, scale, or release evidence. A green
> local suite is necessary but does not satisfy the protected gates.
>
> **Drift check (run first)**:
> `git diff --stat 49bc8f0cc96..HEAD -- crates/crab-cell-app crates/crab-cell-host crates/crab-cell-runtime crates/crab-http-server advisor-plans .github/workflows`

## Status

- **Priority**: P0
- **Effort**: XL
- **Risk**: HIGH — this plan changes the production ownership boundary and the
  release decision for every supported primitive
- **Depends on**: plans 018–024 and an isolated qualification environment
- **Category**: architecture / correctness / performance / operations / release
- **Planned at**: commit `49bc8f0cc96`, 2026-09-20
- **Implementation status**: local runtime, typed application, public host typed
  lifecycle smoke and complete ten-row matrix smoke,
  provider-neutral durability construction/recruitment/rotation, deterministic
  workload with seed-bound lifecycle cases for every primitive, public `CellNode`
  typed all-primitive execution, public
  receipt/matrix validation, protected-profile/run-artifact verifier coverage,
  release negative gates, exact tagged-source profile binding, a canonical
  protected execution-evidence file codec and fail-closed `bind-protected`
  receipt command, a
  schema-2 canonical manifest builder, readiness-frozen host facility
  ownership, and a fresh/clock-skew check for protected matrix consumption are
  implemented and tested; the protected ten-row profile workload and
  provider/Kubernetes/scale receipts remain open

## Next execution slice: honest primitive case evidence

The `public_cell_qualification.rs` test is a typed **smoke**, not a primitive
fault matrix. It now reports measured acknowledgements without manufacturing
retry, rejection, or ambiguity from workload hints. Its lifecycle bitset marks
only observed happy paths and SQL, KV, Blob, Queue, Cron, Workflow, and Effects
duplicate checks (15 of 56 bits); the other scheduled cases remain unmarked.
SQL, KV, Blob, Cron, Workflow, and Effects replay the same mutation identity
and compare the first receipt and outcome; Queue resends the same producer
identity under a distinct request and checks one message and zero remaining
ready or leased work. Workflow also checks the terminal event sequence and
Effects checks the settled lease. KV expiry is
checked separately through the same public typed host because waiting for a
real TTL inside one measured operation can exceed the PR latency threshold.
The separate check does not
claim a workload case bit or protected evidence.
`public_cell_lease_expiry.rs` exercises Blob upload expiry and Queue, Activity,
and source Effect lease expiry through the public typed host. The lease cases
check rejection of the expired token, reclaim with the same identity and a new
token/attempt, final settlement, independent typed observation, and zero
reservations on both in-memory and isolated RustFS storage. Activity also
checks an exact terminal Workflow result and duplicate completion without a
second event. Blob checks that an acknowledged part stays invisible after the
upload expires and a fresh upload can publish exact bytes under the same key.
These are local same-process cases; destination Effect delivery expiry and
protected scheduled expiry evidence remain open.
The workload's precomputed outcome counts are advisory; the run-artifact
validator binds scheduled attempts and validates the measured outcomes. The
local ten-row test still reuses the same primitive workload for every row.
Neither test is protected evidence, including when the smoke runs against
RustFS. Keep these tests useful for wiring, but do not promote their receipts
or case counts.

The smoke now checks observed SQL rows after a durable insert, KV value and
duplicate result, Blob bytes/ETag, Queue message identity and final lease,
Cron schedule, Workflow cancellation, Activity completion, and Effect lease
settlement. The separate KV expiry check reads before and after its TTL; a
duplicate request must retain the first receipt. It also checks the runtime
reservation ledger after `CellNode` shutdown. Its test-only node lease renewal
keeps slow provider smoke alive; it is not a substitute for authoritative fleet
lease publication. On 2026-09-21, the focused in-memory primitive workload,
local ten-row matrix
smoke, and ignored isolated-RustFS primitive workload passed with these
checks. The 15-bit isolated RustFS run passed in 140 seconds after Workflow and
Effects duplicate replay checks were added. The expiry wait remains outside
profile timing. An earlier
run with the TTL wait inside one operation failed the unchanged 5-second PR
latency threshold. Scheduled lifecycle cases and independently checked
protected evidence remain open for every primitive; expiry still needs a
separate case artifact tied to the scheduled operation.
The public Activity capability runs claims and completions inside
`ActivitySupervisor`. It now resolves an ambiguous completion in the request
ledger and retries an absent request with the same identity and result. A
second idle supervisor poll or duplicate Workflow start is not Activity
duplicate evidence.
`ApplicationHandle::resolve` now checks the pending target against the compiled
application and forwards the existing request-ledger lookup. Fault executors
can use it to distinguish a committed command from an absent or still-unknown
attempt before deciding whether to retry; the current smoke does not inject a
lost response or claim a retry bit.

Separate SQL, KV, Blob, Queue, Cron, Workflow, and Effects response-loss tests
dispatch signed peer commands through
the canonical `PeerDispatcher`, drop each reply after dispatch, and check the
returned `PendingMutation` through `ApplicationHandle::resolve`. A distinct
typed client checks exact SQL, KV, and Blob bytes, one claimed and settled Queue
message, Cron schedule generation, terminal Workflow result, or settled Effect
lease, then compares the commit sequence. All seven tests passed on in-memory
and isolated RustFS storage on 2026-09-21 and drained runtime reservations.
The complete seven-case RustFS target also passed concurrently against one
fresh bucket with distinct per-test roots.
They do not replay committed commands or claim retry case bits. Their observers
share a process with the owner. The RustFS fixture now adds a per-process sequence
to each root so parallel cases cannot reuse a prefix. A five-case concurrent
run passed after this change; a prior run had one `Fenced` SQL bootstrap and
conditional-write conflicts, while a serial diagnostic passed all five.

`public_cell_retry.rs` now drops the first signed mutation request before
dispatch for SQL, KV, Blob completion, Queue send, Cron upsert, Workflow start,
and Effect acknowledgement. Each caller receives `PendingMutation`, resolves
`Absent`, retries the same identity once, and checks one dispatched mutation
and exact state through a separate typed client. Queue and Effects also check
final settlement; every node drains to zero reservations. All seven cases
passed together on in-memory and fresh isolated RustFS storage on 2026-09-21.
The sibling response-loss cases were rerun on the same fresh RustFS bucket and
also passed. `public_cell_activity_retry.rs` now covers two claim boundaries:
a pre-dispatch loss resolves `Absent` and the next supervisor run completes
once; a post-dispatch lost response resolves `Committed`, the 5-second lease
expires, and the next run reclaims and completes the Activity once. Both
passed on in-memory and fresh RustFS storage on 2026-09-21, with an
independent typed Workflow read, an idle follow-up, and zero reservations.
Four Activity fault cases now cover pre-dispatch and post-dispatch loss at both
claim and completion. The completion cases check an exact same-identity retry
after `Absent`, or return the committed result after `Committed`, without
rerunning the handler. All four passed in memory and on fresh isolated RustFS
on 2026-09-21. If completion resolution remains `Unknown`, expires, or fails,
`run_once` still returns pending evidence without a public exact completion
replay path; durable handoff for that result remains open. These tests share a
process with their observer and are not scheduled matrix cases or protected
provider evidence, so they do not set retry or expiry case bits.

`PreparedCommand` retains exact encoded typed input, identity, digest, and owner
incarnation before dispatch, so a cancelled caller can resolve the attempt
through `ApplicationHandle::resolve`. The public host now cancels signed peer
mutations immediately before and after dispatch for SQL, KV, Blob completion,
Queue send, Cron upsert, Workflow start, and Effect acknowledgement. A separate
typed application handle resolves the retained evidence, checks absence before
an exact-identity retry or observes the committed result, then checks the exact
row, value/version, Blob bytes/size/ETag, message and final Queue lease,
schedule generation, terminal Workflow result, or settled Effect lease. All
fourteen cases passed in memory and on isolated RustFS on 2026-09-21; each node
drained runtime reservations. Four more cases cancel the public typed Activity
claim and completion commands before and after signed-peer dispatch. A separate
typed application handle resolves the exact request, validates the claim lease,
checks a terminal Workflow result and event sequence, and verifies duplicate
completion leaves one transition. All four passed in memory and on isolated
RustFS on 2026-09-21 with zero reservations. Two destination Effect cases cancel
the signed delivery before and after inbox dispatch. The retained source claim
resolves the destination inbox, retries only an absent delivery, and checks an
exactly-once typed SQL row, identical inbox replay result, source settlement,
and zero reservations. Both passed in memory and on isolated RustFS on
2026-09-21. Cancellation while the native Activity handler or Effect supervisor
executes, independent-process observation, and protected scheduled matrix
evidence remain open. These tests share a process with their observer, so no
cancellation case bit is set yet.

A separate `public_cell_takeover.rs` test now uses a fresh successor `CellNode` and
empty local directory against the same object store. It checks the exact
published root and a higher owner epoch for the six owned Cells. Typed
successor reads confirm acknowledged SQL, KV, Blob, Cron, and Workflow state;
the successor claims and settles the pending Queue message, Activity, and
Effect, then checks their final state and leases. Stale source writes are
rejected for every owned Cell and both nodes drain to zero reservations. The
in-memory and isolated-RustFS versions passed on 2026-09-21. This is a
test-controlled session fence while the source process remains alive; it is
not a protected owner-kill or a scheduled qualification case. The process
fault test below exercises an independent successor, but raw artifacts and
protected provider evidence remain necessary before owner-loss/recovery case
bits can be claimed.

`public_cell_process_fault.rs` now kills a separate owner process on isolated
RustFS after acknowledged SQL, KV, Blob, Queue, Cron, and Workflow writes,
plus acknowledged Workflow starts that schedule Activity and Effect work.
The successor starts with an empty local directory and checks exact SQL bytes,
KV bytes/version, Blob bytes/ETag/size, Queue message ID/payload, Cron
generation/due time, completed Workflow result/event sequence, each source
commit sequence, exact restored roots, and higher owner epochs. Before settling
pending work, it replays the original SQL insert, KV put, Blob completion,
Queue send, Cron upsert, and Workflow start with their exact mutation identities
through typed capabilities. Each returns the original commit sequence and
outcome; the independent reads still show one SQL row, the first KV version,
one Blob, one Queue message, the first Cron generation, and one Workflow event.
These are real-storage cross-process duplicate checks, but they do not set
protected matrix case bits. The successor claims and
acknowledges the Queue message, completes the pending Activity, delivers the
Effect to its SQL destination through the signed peer inbox, then settles the
source lease. After the recovered Cron schedule becomes due, the successor
executes its typed maintenance Tick, observes occurrence one and the next due
time, delivers the resulting Effect through the signed peer inbox, and checks
one exact SQL occurrence row, identical replay and inbox resolution, and a
settled source lease. A repeated Tick produces no second occurrence. The
successor checks no remaining claims and zero runtime reservations. A second
boundary kills the owner before those writes;
the successor checks all six values/schedules and both pending work items are
absent. A third boundary kills the owner after Queue, Activity, and source
Effect claims acknowledge. The independent successor rejects all three stale
tokens after expiry, reclaims the same work on attempt two with new tokens,
checks exact Activity result bytes and one terminal Workflow event, delivers
the reclaimed Effect exactly once to SQL, settles Queue and Effect leases,
and drains reservations. All three concurrent RustFS
cases passed on 2026-09-21. The test uses a test-controlled
session fence. It is not a scheduled qualification case or protected
three-process provider run, and it does not set matrix case bits. Activity
completion and Effect destination delivery happen after takeover; their
pre-kill acknowledgements cover scheduling, not settlement.

Implement one fault-capable executor through `CellNode` and typed
`ApplicationHandle` capabilities. Each operation writes a unique, bounded
marker; an independent reader or successor checks the exact value, version,
status, sequence, or effect/lease ledger after the fault. Do not set
`QualificationExecution::verified(true)`, a retry count, or a case label until
those observations exist. Unimplemented cases return an error instead of
claiming coverage. Record fault-injection boundary and observation in raw
artifacts; the bounded receipt cites their digests.

| Primitive | Independent state and settlement check |
| --- | --- |
| SQL | Query the unique row/value after each command, then query it again from the successor's restored root. Compare receipt sequence and reject duplicate inserts/updates beyond one application. |
| KV | Read the exact scope/key/value/version; check expired keys are absent and a repeated request preserves the first version. |
| Blob | Read the entire published byte range and ETag; check incomplete/expired uploads stay invisible and no part or upload reservation remains. |
| Queue | Check producer deduplication, exact message ID/payload/attempt, lease expiry/reclaim, final Ack/Dead state and zero live leases. |
| Cron | Read schedule generation, enabled flag, next due time and occurrence; check one durable Tick effect per occurrence after replay or takeover. |
| Workflow | Read run ID, status, event sequence, result and retained definition; check no duplicate transition and no pending local work after terminal cancellation. |
| Activity | Check exact claim token/attempt, retry or expiry outcome, completion result in the workflow run, and no live activity lease. |
| Effects | Check exact source effect ID/attempt/token, target inbox deduplication, final delivered/failed state and no live source lease. |

For **each** row above, exercise the same six lifecycle boundaries:

1. Retry: lose or delay the first response after commit or publication, resolve
   its request ID, then retry only if the resolved state allows it. Verify one
   logical effect and record the actual attempt count.
2. Duplicate: submit the same mutation identity or producer/event identity
   twice, compare the durable outcomes, and verify no second side effect.
3. Expiry: expire the primitive's lease/TTL when it has one; also submit an
   expired mutation identity. Verify rejection or reclaim and the final state.
4. Cancellation: cancel at an injected await boundary, resolve the request ID
   from a separate client, and verify either one committed outcome or no
   outcome. Drain the operation and check reservations.
5. Owner loss: kill the owner before and after publication, seal/recover its
   node log, fence the stale session, and confirm that the old owner cannot
   produce output.
6. Recovery: start a successor with an empty local Cell directory, restore the
   exact authoritative root plus any pinned tail, and recheck the primitive's
   marker and lease/work state through its public capability.

Run these cases first on an isolated real object-store prefix with a
three-process native host; then repeat in the protected provider and Pod
profiles. Use one unique namespace/prefix per run and retain immutable raw
events outside Git. A case passes only when the fault is observed at its named
boundary, a separate observer confirms the result, acknowledged outcomes are
present exactly once, authority/root sequence is monotonic, and every Cell,
job, activity, Queue, Effect, and restore reservation returns to zero after
drain. A timeout, missing observer, or unresolved ambiguous operation fails
the case. Run the receipt verifier in a fresh process; never infer a case from
the deterministic schedule or a successful API call alone.

## Decision boundary

The audit verifies source behavior and executable local tests. It does **not**
support an unconditional claim that the runtime is production-ready for every
large-scale distributed system. Readiness must be stated for a named profile
(source/image, provider, topology, Cell cardinality, workload, duration,
resources, and thresholds) after the exact-candidate matrix passes. The
following semantic exclusions remain part of the supported contract:

- one command is atomic within one Cell; there is no multi-Cell ACID;
- Queue and external Activity/Effect delivery are at-least-once, so destination
  idempotency is required; Queue ordering is not a FIFO promise;
- SQL, batches, Blob parts/objects, Workflow history/state, Activity payloads,
  and Effect batches are bounded rather than unbounded general-purpose stores;
- Cron is a durable fixed-interval scheduler, not a timezone/general-cron
  expression engine;
- hot-Cell splitting and transparent live resharding are application concerns.

## Verified source baseline

The current branch already proves the following locally; preserve these checks
while closing the remaining gaps:

- `crates/crab-cell-app/src/lib.rs:264-282` validates the compiled namespace,
  registry, and typed capability module. `:344-367` rejects a command/query
  whose target belongs to another module before transport; the app integration
  test exercises this boundary through the full-primitive router.
- `crates/crab-cell-host/src/lib.rs:535-623` requires a task group, lease, and
  declared owned components before readiness. `:655-700` validates an entire
  facility batch before retaining any item, so composition failure is atomic.
- `crates/crab-http-server/src/server.rs:1127-1168` installs the production
  router, peer receiver, node-log transport, publisher, catalog, scheduler
  status, release store, and capacity report as one host-owned batch.
  `:1263-1365` retains the coordination loops in the host task group and only
  opens readiness after startup probes and lease installation.
- The provider boundary is now explicit in
  `crates/crab-http-server/src/peer.rs:245-298`: the server-side publisher
  enrolls the node session and returns `NodeDurabilityConfig`, while
  `crates/crab-cell-host/src/lib.rs` builds, installs, recruits, rotates, and
  joins the runtime durability object. The server no longer constructs
  `NodeDurability` or owns a second recruitment/rotation loop. Keep provider
  credentials, HTTP transport, and authority enrollment at the edge; do not
  move those into the runtime or add another scheduler. The architecture gate
  now rejects direct server construction of `DurabilityGate`, `NodeDurability`,
  and `NodeLogShipper` as well as direct `CellRuntime` construction.
- `crates/crab-cell-runtime/src/qualification.rs:20-38` defines the schema and
  ten matrix rows; `:40-61` defines the schema-2 provider/topology and
  throughput/RSS/disk/FD/object-store-call envelope. Built-in protected profiles
  exist at `:122-191`, but local `emit` output is not protected evidence.
  The release workflow requires an exact protected matrix at
  `.github/workflows/http-server-release.yml:252-284`.
- `crates/crab-cell-runtime/src/peer.rs` and `src/peer/dispatch.rs` execute
  native primitives only through registered `CellCommand`/`CellQuery` codecs.
  The private `docs/contracts/peer.proto` now reserves the former direct
  SQL/KV/Queue/Workflow/Activity wire fields, so the checked-in protocol cannot
  advertise a second path that the verifier rejects.

## Primitive contract matrix (source-backed)

Use these implementation limits and delivery semantics in fixtures, profile
workloads, and published support claims. If a proposed use case needs a
different contract, stop and split it into a reviewed API plan.

| Primitive | Source contract to qualify | Evidence |
| --- | --- | --- |
| SQL | Parameterized bounded batches (128 statements), at most 1,000 rows and 1 MiB operation/result; protected runtime tables are denied by the authorizer. | `crates/crab-cell-runtime/src/sql.rs:14-18`, `:168-204`, `:284-313` |
| KV | Values and keys are bounded (4 MiB value, 1 KiB key), atomic batches are capped at 128 items and 4 MiB plus 64 KiB of input, and list pages are bounded. | `crates/crab-cell-runtime/src/kv.rs:15-24`, `:225-274`, `:307-355`, `:408-421` |
| Blob | Multipart parts are 256 KiB, at most 4,096 parts (1 GiB object ceiling), reads are range-bounded, and uploads have a seven-day maximum lifetime. | `crates/crab-cell-runtime/src/blob.rs:11-20`, `:280-304`, `:420-433`, `:510-545` |
| Queue | Payloads are 256 KiB, claims are capped at 32 items/512 KiB, leases and attempts are bounded, producer deduplication exists, delivery is at-least-once, and selection is by due time/message ID rather than a FIFO guarantee. | `crates/crab-cell-runtime/src/queue.rs:18-31`, `:211-263`, `:274-372` |
| Cron | Durable schedules use a fixed interval between 1 second and 365 days, with bounded payload/future windows and durable Tick effects; timezone/general-cron expressions are not implemented. | `crates/crab-cell-runtime/src/cron.rs:12-17`, `:142-203`, `:250-322` |
| Workflow | Deterministic transitions persist bounded state/events/effects: 1 MiB workflow bytes, 128 actions, and 100,000 events per Cell. | `crates/crab-cell-runtime/src/workflow.rs:34-38`, `:258-407`, `:763-810` |
| Activity | Native activity attempts have 256 KiB input/output, claims are capped at 32/512 KiB, leases are 5–300 seconds, and completion is exact-token/attempt checked with retries and duplicate outcomes. | `crates/crab-cell-runtime/src/workflow/activity.rs:10-20`, `:58-86`, `:88-120`, `:308-376` |
| Effects | One command may emit at most 128 effects and 1 MiB total; source leases/acks are at-least-once and destination inbox deduplication is required for external side effects. | `crates/crab-cell-runtime/src/effects.rs:20-36`, `:140-205`, `:251-320` |

## Scope

**In scope**:

- moving NodeDurability/recovery/publisher construction and ownership behind the
  provider-neutral `CellNode` host while retaining provider credentials,
  transport construction, authentication, and HTTP policy at the product edge;
- completing application descriptor semantic validation and an executable
  fixture matrix for operation, migration, role, effect, dead-letter,
  Workflow, Activity, and resource-limit contracts;
- driving all ten qualification rows through typed host/application APIs and
  recording bounded metrics plus immutable raw artifacts; protected primitive
  run artifacts bind RSS, disk, file-descriptor, and object-store-call
  measurements to the signed receipt;
- protected three-process provider, dedicated scale, Kubernetes fault, every
  advertised object-store provider, and rolling-compatibility receipts;
- release-gate negative tests, evidence freshness, exact image/source/profile
  binding, and operations documentation derived from measured envelopes.

**Out of scope**:

- a second receipt/report format, direct SQL/SQLite qualification bypass,
  arbitrary transactions, transparent resharding, exactly-once external work,
  or a generic Queue consumer callback;
- inventing universal throughput/latency numbers, adding credentials to Git or
  logs, changing the provider contract to make a test pass, or silently adding
  compatibility aliases/fallback readers;
- changing unrelated `crab` CLI/product behavior or fixing a runtime defect in
  the qualification harness. Return such a defect to the owning plan.

## Execution steps

### 1. Reconfirm the source and local baseline

Map each required facility to its constructor, owner, cancellation token, drain
callback, and final join. Add no new owner until the map is complete. Confirm
that the typed reference app cannot invoke a capability against a different
module and that readiness cannot open with a missing facility.

**Expected proof**: the focused app/host tests pass and the source map names one
owner for runtime, authority, publisher, durability, follower store, scheduler,
effects, activities, telemetry, and shutdown.

### 2. Finish host ownership and lifecycle

Add a provider-neutral host operator bundle/supervisor API. `CellNode` must
construct or receive already-constructed provider-neutral durability, recovery,
publisher, peer, scheduler, effect, and Activity supervisors, retain them, and
join them through one bounded task/facility group. Provider SDKs, credentials,
TLS identity, HTTP routes, and node-session enrollment remain product-owned
inputs. The canonical path now has the host build and own NodeDurability from a
provider-returned configuration; delete any old server construction path rather
than adding another scheduler.

Add a public test cluster that uses only application/host APIs. It must prove
startup, readiness, admission stop, reverse facility drain, task join, lease
fencing, and zero runtime reservations after shutdown. A fault-injection hook
may be test-only; it must not become a production fallback.

**Expected proof**: an ownership/lifecycle test fails if any facility is
constructed outside the host, if two schedulers/authorities exist, or if a
background task survives the drain deadline.

### 3. Close the application contract

Complete semantic validation from the current typed source of truth. Add
table-driven tests for duplicate IDs/names, role/module mismatch, migration
ordering/digest changes, missing effect and dead-letter targets, missing
Workflow definitions/Activity support, invalid shard/partition settings, and
every declared limit. Keep generated clients out of this step unless a separate
approved design identifies the descriptor as their sole source of stable IDs.

**Expected proof**: the full-primitive reference application compiles one
canonical descriptor and one negative fixture per relationship; no test imports
server internals or opens SQLite directly.

### 4. Implement measured typed qualification

Extend the existing deterministic workload adapter and its versioned run
artifact (the receipt schema remains unchanged) to run the matrix through
`ApplicationHandle` and `CellNode`. Exercise every row
and every primitive with happy, retry, duplicate, expiry, cancellation,
owner-loss, and recovery cases relevant to the contract. Record attempted,
acknowledged, rejected, ambiguous, retried, and verified counts; p50/p95/p99;
throughput; RSS/cgroup memory; local disk; file descriptors; worker/Cell/job
reservations; object-store calls; backlog/history/object cardinalities; owner
epochs; exact roots; and cleanup residuals.

Raw events belong in an external artifact with bounded labels. The receipt must
bind source SHA, image digest, profile digest, provider, topology, seed, fault
schedule, and every raw-artifact digest. Same seed/profile must reproduce the
logical outcome digest; a changed seed must change the workload identity.

**Expected proof**: the PR contract remains fast and deterministic; protected
profiles fail closed when measured fields are absent or exceed their envelope.

### 5. Establish and run protected profiles

Freeze thresholds before the candidate run on recorded hardware/images. Run the
local three-process provider tier first, then the dedicated scale tier, then
three-or-more-Pod fault cases, provider-specific semantics for every advertised
provider, and rolling compatibility. Isolate every bucket/prefix/namespace and
retain raw artifacts outside Git. Prove fault injection occurred at the named
boundary, including owner kill before/after publication, peer partition/latency,
lost CAS/release response, follower/source loss, throttling, disk pressure,
Activity lease expiry, and rolling image replacement.

**Expected proof**: each of the ten rows has one exact-candidate protected
receipt; acknowledged outcomes resolve without divergence, one authoritative
owner remains, roots never regress, and shutdown returns reservations to zero.

### 6. Enforce the release decision and publish the envelope

Add negative coverage for missing, partial, stale, dirty, unsigned, forged,
wrong-provider, wrong-image, wrong-profile, wrong-seed, and wrong-source
receipts. Keep release blocked until the exact protected matrix verifies in a
fresh process. Update operations/support docs only from accepted receipts and
state the tested topology, provider, cardinality, resource limits, SLOs, alarms,
restore/failover procedure, and semantic exclusions.

**Expected proof**: a release candidate cannot package fixture receipts or a
  receipt from a different source/image/profile, and published claims equal the
  measured profile rather than aspirational limits.

## Verification commands

Run each Cargo command with a checkout-specific target directory under
`$HOME/Workspace/crabbuild-target`; stop if that mounted volume is unavailable.
`RUSTC_WRAPPER=` is intentional for deterministic local verification.

| Purpose | Command | Expected result |
| --- | --- | --- |
| Application contract | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-app RUSTC_WRAPPER= cargo test -p crab-cell-app --locked` | all unit and reference-application tests pass |
| Host ownership | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-host RUSTC_WRAPPER= cargo test -p crab-cell-host --locked` | lifecycle/facility tests pass with no leaked tasks |
| Public typed primitive host | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-public-host RUSTC_WRAPPER= cargo test -p crab-http-server --test public_cell_host_application --test public_cell_qualification --test public_cell_response_loss --test public_cell_retry --test public_cell_activity_retry --test public_cell_takeover --test public_cell_process_fault --locked` | typed smoke, response-loss reconciliation, absent-request retries, Activity claim fault recovery, fresh-successor takeover, and process harness tests pass; ignored RustFS cases require the isolated provider job |
| Runtime regression | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-runtime RUSTC_WRAPPER= cargo test -p crab-cell-runtime --locked` | all runtime tests pass; ignored provider tests are reported, not fabricated |
| Server composition | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-server RUSTC_WRAPPER= cargo test -p crab-http-server --lib --locked` | server library tests pass |
| Strict lint | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-quality RUSTC_WRAPPER= cargo clippy -p crab-cell-app -p crab-cell-host -p crab-cell-runtime --all-targets --locked -- -D warnings` | exit 0 |
| Architecture | `python3 crab/scripts/check-architecture-gates.py` | exit 0 |
| Documentation links/schema | `node crates/crab-cell-runtime/docs/validate.mjs` | schema and link checks pass |
| Release negatives | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-receipts RUSTC_WRAPPER= cargo test -p crab-cell-runtime --test qualification_receipt --locked` | all mismatch classes reject |

Before any protected run, inspect the live qualification CLI/script help and
record the exact command in the receipt. Never guess provider or Kubernetes
flags. Never run bucket-wide GC.

## Machine-checkable done criteria

- [x] One `CellNode` owns construction, retention, drain, and join for every
  production runtime/operator facility; provider-specific enrollment and HTTP
  transport remain explicit edge inputs, with no server-side duplicate
  durability owner.
- [x] Host lifecycle tests prove readiness gating, admission-before-drain,
  reverse facility drain, task joining, lease fencing, and zero reservations.
- [x] Application semantic-validation fixtures cover every relationship listed
  in step 3 and all full-primitive calls remain typed and module-safe.
- [x] The ten-row local matrix smoke is driven only through public typed
  host/application APIs and binds its measured fields and raw-artifact digests;
  protected profile execution remains open.
- [x] Same seed/profile reproduces the logical qualification digest and a
  changed seed changes it; labels remain bounded.
- [x] Protected primitive run artifacts require bounded resource metrics and
  the verifier rejects any receipt whose signed resource measurements differ
  from the measured artifact.
- [ ] Every required primitive fault/retry/duplicate/expiry/cancellation/
  owner-loss case passes for the named protected profile.
- [ ] Mixed scale thresholds pass on fixed recorded hardware/provider without
  lost outcomes, owner overlap, root regression, or leaked reservations.
- [ ] Three-or-more-Pod Kubernetes faults and every advertised provider pass
  their own exact-candidate receipt.
- [ ] Release rejects all mismatch classes and accepts only a complete,
  signed, fresh matrix for the exact source/image/profile.
- [ ] Support/operations claims cite measured profile data and retain the
  semantic exclusions above.

## STOP conditions

- No dedicated provider prefix, fixed scale environment, isolated Kubernetes
  namespace, or authorized evidence store is available.
- Credentials, private endpoints, or secrets would enter logs, receipts, raw
  artifacts, Git, or the PR.
- A fault cannot be demonstrated at the requested boundary, or a result cannot
  be independently checked in a fresh process.
- A threshold is selected or relaxed after observing the candidate result.
- Any acknowledged operation is lost/divergent, two authoritative owners are
  observed, an exact root regresses, or a reservation leaks.
- A runtime correctness/performance defect appears; stop the harness work and
  return the defect to the owning plan.
- A protected row is represented only by a fixture, local emulator, or synthetic
  receipt.

## Maintenance notes

Evidence expires when source, image, workload, provider semantics, topology,
resource limits, or a relevant invariant changes. New cardinality/provider/SLO
claims require a new profile digest and signed run. Keep plans 022–024 marked
partial until this plan's protected gates are genuinely complete; local green
tests alone must not be converted into a universal production-ready claim.
