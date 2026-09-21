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
  release negative gates, exact tagged-source profile binding, a
  schema-2 canonical manifest builder, readiness-frozen host facility
  ownership, and a fresh/clock-skew check for protected matrix consumption are
  implemented and tested; the protected ten-row profile workload and
  provider/Kubernetes/scale receipts remain open

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
| KV | Values and keys are bounded (64 KiB value, 1 KiB key), atomic batches are capped at 128 items/1 MiB, and list pages are bounded. | `crates/crab-cell-runtime/src/kv.rs:15-23`, `:313-350`, `:357-408` |
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
| Public typed primitive host | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-025-public-host RUSTC_WRAPPER= cargo test -p crab-http-server --test public_cell_host_application --test public_cell_qualification --locked` | one `CellNode` boots every primitive Cell and the typed workload artifact verifies |
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
