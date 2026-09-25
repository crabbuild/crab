# Plan 035: Keep the Cell runtime safe and fast as a reusable distributed-service framework

> **Executor**: Read root `AGENTS.md`, `crates/AGENTS.md`, and the scoped guides
> for every crate you touch. Execute one slice per reviewable change. Do not
> mark a slice complete from prose, a mock-only test, or an unsigned local
> qualification result. Update the status table in `advisor-plans/README.md`
> after each slice. Do not push or open a PR unless instructed.
>
> **Drift check, before each slice**:
> `git diff --stat 3b8d3b3614c..HEAD -- crates/crab-cell-runtime crates/crab-ltx crates/crab-cell-app crates/crab-cell-host crates/crab-http-server/src/cells crates/crab-http-server/src/local_disk.rs .github/workflows advisor-plans`
> Re-read changed contracts and their callers before executing a step. If an
> earlier plan has already supplied the same proof, record its exact test and
> receipt instead of adding a second implementation.

## Status and objective

- **Priority**: P0 for durability and capacity proof; P1 for measured optimization.
- **Effort**: XL program, six bounded slices; **risk**: HIGH at ownership,
  publication, restart cleanup, and public framework boundaries.
- **Planned at**: `3b8d3b3614c`, 2026-09-25.
- **Status**: IN PROGRESS. This plan coordinates unfinished work; it does not reset
  progress recorded by plans 006, 009, 012, 015, 024, 025, 032, or 034.
- **Goal**: A Cell application can use the same runtime on thousands of Cells
  without losing an acknowledged result, serving from stale authority, leaking
  node capacity across restarts, or relying on an unmeasured performance claim.
  Every advertised scale profile has a fresh, source-bound qualification receipt.

| Slice | Exit artifact | Depends on | Status |
| --- | --- | --- | --- |
| 1. Durability seam | Real-path fault matrix and retained seeds | Current protocol | LOCAL MATRIX COMPLETE — named seams have production-path outcomes and reproducible fault records; protected CI replay remains open |
| 2. Resource envelope | 1k/5k/10k measurement and restart accounting | Plan 012 | PARTIAL — bootstrap host admission and local churn accounting proved; protected scale receipt checks marginal open-Cell slopes against admission; fixed overhead, actual slopes, and restart receipts open |
| 3. Hot and many-Cell cost | Paired phase/cost receipts | Slice 2; plan 034 | TODO |
| 4. Hydration and retention | Promotion/fault/reopen receipts | Plans 009, 015, 032 | PARTIAL — resident zero-origin, origin/disk/lease-loss and in-flight takeover fences, shutdown reservation, and offline current/pinned/compacted root reopen tests pass; local RustFS takeover and retention pass; large-Cell/protected provider receipts open |
| 5. Framework reuse | Second application source-loss proof | Slice 1; plan 024 | PARTIAL — six typed modules and two SQL shards survive `CellNode` source-loss takeover, including local RustFS; descriptor storage limits bind node admission and the operator API audit is complete; protected receipt open |
| 6. Continuing gates | PR, scheduled, and protected evidence | Slices 1–5 | PARTIAL — simulator and typed PR gates corrected, scheduled replay and fuzz evidence retained, release promotion follows protected verification; protected receipt open |

## Current state and fixed contracts

`crab-cell-runtime` is the embedded owner of Cell identity, control/CAS,
single-Cell actor and SQL worker, publication, follower durability, fleet
resource admission, and qualification. `crab-ltx` owns managed SQLite capture,
exact-root object mechanics, and sparse hydration. `crab-cell-app` owns author
registration; `crab-cell-host` and `crab-http-server` compose providers and
product policy. Do not move HTTP, credentials, or provider construction into
the runtime (`crates/AGENTS.md`, `crates/crab-cell-runtime/AGENTS.md`).

The non-negotiable response rule is:

```text
response(commit) => object_root_covers(commit) OR fleet_covers(commit)
```

The follower proof requires every selected member to durably cover its ticket;
takeover must consume a fleet-only tail before serving. The actor serializes
root publication and retains pending cuts until exact confirmation
(`crates/crab-cell-runtime/docs/failover-and-followers.md`,
`crates/crab-cell-runtime/src/cell/actor/requests.rs:258`,
`crates/crab-cell-runtime/src/publication.rs`). One Cell has one writer and
one consistency domain. Cross-Cell effects and activities are retryable work,
not a transaction spanning Cells (`crates/crab-cell-runtime/docs/application-framework.md`).

Existing protections must be used rather than rebuilt:

- `src/coordination/sim.rs` replays pure protocol events and historical seeds;
  `tests/runtime/publication.rs` drives real `Db`, `CellReplica`, and
  `CellPublisher`, including a lost CAS response. The pure simulator alone
  cannot prove the capture-to-CAS seam.
- `src/fleet/resource.rs` charges 64 KiB and eight descriptors per active Cell;
  `crab-ltx::Db` retains three connections with a 64 KiB page-cache target
  each. These are admission constants, not a measured 10,000-Cell envelope.
- `src/cell/executor.rs` bounds pending publication to 64 cuts and 64 MiB.
  The publication-cost telemetry and local RustFS benchmark exist; cloud and
  many-Cell tails remain to be qualified.
- `crates/crab-http-server/src/local_disk.rs:75` reserves bytes from earlier
  process sessions at startup and rejects symlinks/special files. It does not
  reclaim those quarantined bytes.
- `src/recovery/retention.rs` collects immutable objects **offline** under an
  exclusive maintenance fence. Do not invent an online sweep protocol.
- `qualification/profiles/scale-v1.json` currently requires 10,000 Cells,
  10,000,000 operations over 3,600 seconds, at least 2,777 operations/s,
  p99 at most 500 ms, and the recorded RSS/disk/descriptor/call ceilings.
  Use the tracked profile as authority; never relax it to pass a run.

The runtime is functionally implemented but lacks complete protected capacity,
provider, and multi-Pod fault qualification
(`crates/crab-cell-runtime/docs/delivery.md:3`). The current plan owners are:

`typed_application_executes_every_primitive_through_a_local_router` in
`crates/crab-cell-app/tests/reference_application/primitives.rs` already
drives several typed modules through `CompiledApplication` and restores their
published roots into a fresh runtime after deleting local SQLite sources. It
also runs an activity and effects before and after recovery. The test now
checks the recovered KV value, blob bytes, and completed workflow state
exactly.
`public_cell_node_primitive_takeover_preserves_acknowledged_values` in
`crates/crab-http-server/tests/public_cell_takeover.rs` composes the reference
application through two `CellNode`s, restores six Cell types and seven Cells
into an empty successor directory, checks exact SQL/KV/Blob/Queue/Cron/Workflow
results and Activity/Effect settlement, then rejects stale-owner writes. Both
SQL shards have distinct acknowledged rows, recovered local files, and
post-takeover fencing checks. The protected application receipt remains open.
Do not duplicate this proof in `crab-cell-host` tests.

| Work already owned | Existing plan | Rule here |
| --- | --- | --- |
| Metadata lookup, due-work discovery, pipelined publication, dormant resume | 034 | Finish or consume its receipts before tuning these paths |
| Hydration and resident promotion | 009 | Qualify the existing path; do not build a second downloader |
| Resource ledger and startup inventory | 012 | Calibrate and close the lifecycle gap at its owner |
| Primitive, provider, scale, and protected receipts | 015, 024, 025 | Extend the canonical harness and profile schema only |
| Rebalance and three-node movement | 032 | Consume its protected proof; do not create another controller |
| Pure simulation and model checking | 006, 007 | Extend retained seeds/model when protocol behavior changes |

## Commands and execution rules

Use a distinct external target directory for this checkout. Before compiling,
verify `$HOME/Workspace` is mounted and writable. Set `CARGO_TARGET_DIR` on
**each** Cargo invocation; never allow a local `target/` build. The commands
below use this checkout's `crab-fd9c` directory. Substitute a different unique
name when executing in another worktree.

| Purpose | Command | Required result |
| --- | --- | --- |
| Runtime tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo test -p crab-cell-runtime --features test-support --locked` | exit 0 |
| LTX contract | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo test -p crab-ltx --features replica --locked` | exit 0 |
| Framework consumer | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo test -p crab-cell-app -p crab-cell-host --locked` | exit 0 when touched |
| Runtime lint | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo clippy -p crab-cell-runtime --all-targets --features test-support --locked -- -D warnings` | exit 0 |
| LTX lint | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo clippy -p crab-ltx --all-targets --features replica --locked -- -D warnings` | exit 0 |
| Format and layout | `cargo fmt --all -- --check` and `python3 crab/scripts/check-cell-ltx-layout.py` | both exit 0 |
| Design contracts | `node crates/crab-cell-runtime/docs/validate.mjs` | exit 0 |
| Receipt contract | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-fd9c cargo test -p crab-cell-runtime --test qualification --locked` | exit 0 |

Run focused tests while developing. Run affected crate and direct-consumer
checks before handoff. Full provider, Kubernetes, 10,000-Cell, and release
proof runs belong in the dedicated qualification environment, not a laptop.
Keep the raw evidence and the first actionable failure. Do not edit a
baseline, profile, snapshot, inventory, or expected-failure file to hide it.

Local verification on 2026-09-25: runtime `--features test-support`, LTX
`--features replica`, app/host tests, runtime/LTX/app Clippy with `-D warnings`,
format, layout, and docs validation all passed in the `crab-fd9c` external
target. The full app/host suite was rerun after the reference application's
recovery assertions changed. These checks are on uncommitted working-tree code
and are not signed scale, provider, or release receipts.
The runtime suite was rerun after the follower-CAS and origin-failure tests;
the 125 runtime integration tests passed. The hydration fault test uses a
dedicated disk budget because `crab-ltx::Host::default()` shares a process-wide
budget with other concurrent tests. The focused retention test passed after
the current/pinned reopen and unpublished-incarnation checks were added.
The coordination workflow PR filter now points at the tracked
`src/coordination/sim.rs`; the retired `src/coordination_sim.rs` path could not
trigger when only the simulator changed. The edited workflow parses as YAML.
The qualification contract PR job now runs `public_cell_takeover`,
`public_cell_process_fault`, and `public_cell_effect_delivery_expiry` alongside
its other typed Cell suites. The edited workflow parses as YAML.
The architecture preflight initially failed on an unchanged `crab-ltx` doc
comment containing the retired type name `Replica`. Its standalone-LTX guard
now ignores standalone Rust line comments while still rejecting callable
retired symbols and storage paths. The focused guard tests and complete
architecture preflight pass, as do policy-entry-point and Rust-fence checks.
The local seven-Cell, two-SQL-shard source-loss takeover and the shared public
Cell qualification fixture passed after the reference descriptor changed.
These in-memory object-store runs do not provide a protected application or
provider receipt.
All ten other `crab-http-server` integration suites that include the shared
fixture passed their non-ignored tests after the descriptor change, including
process-fault and lost-response suites. Focused server Clippy with
`-D warnings`, format, layout, docs validation, and `git diff --check` passed.
After the hydration and representation-compaction tests, the full runtime
`test-support` suite passed again: 353 unit tests and 127 runtime integration
tests passed, with one and two ignored respectively. Runtime Clippy with
`-D warnings` also passed.
After binding application storage limits, the combined app/host/runtime test
run passed, as did the focused public host, qualification, and source-loss
takeover suites. The mismatch tests reject an oversized replica and a recovery
store before control changes. Clippy across all targets of runtime, app, host,
and HTTP server passed with `-D warnings`; format, layout, docs validation,
architecture preflight, policy entry points, and diff whitespace checks pass.
The subsequent lost-ack receipt test and eleven local-disk tests passed;
runtime all-targets Clippy and HTTP server library/tests Clippy passed with
`-D warnings`, as did format and diff whitespace checks.
After adding the scale sample artifact gate, 39 qualification unit tests and
13 qualification integration tests passed. Runtime all-targets Clippy,
format, layout, docs validation, and diff whitespace checks passed.
On 2026-09-25, isolated prefixes in the existing `crab` bucket on local
RustFS 1.0.0-rc.1 passed the ignored runtime mixed-primitive churn and
source-loss takeover tests, the ignored offline retention test, and the
ignored public `CellNode` typed takeover test. A separate
96 MiB `crab-ltx` CellReplica workload initially failed on its second 32 MiB
write with `Limit(LtxFileBytes)`: incremental admission counted newly grown
pages twice. The corrected page count passed a failing-before-fix local
regression and the same live workload: six LTX segments, source deletion,
exact restore, and full-range compaction with matching BLAKE3 and length.
These are local macOS loopback RustFS results, not a signed provider,
scale, Kubernetes, or release receipt.
The separate `crab-http-server/examples/compose` reference service passed a
Docker Compose expansion from 3 to 5, 10, and 20 containers against a RustFS
container. Docker enforced 1 vCPU and 1 GiB per node; each node kept one
acknowledged KV Cell value across later expansion stages, and the final
isolated prefix contained 280 objects. The service uses tenant-partitioned
Cells and a RustFS-backed session lease. It does not exercise the production
server's 2 GiB/20 GiB admission, Cell migration, remote peer dispatch, or
follower durability, so it is not the protected many-Cell resource receipt.

## Slice 1 — Prove the real durability seam under deterministic faults

**Owner and scope**: `crates/crab-cell-runtime/tests/runtime/publication.rs`,
`tests/runtime/lifecycle/durability/{proofs,recovery}.rs`, their existing
`tests/support/` fixtures, and only the production path a failing test proves
wrong. Keep `src/coordination.rs` pure. Touch `crab-ltx` only if the failure is
inside capture or exact-root mechanics.

1. Extend the real `Db`/`CellReplica`/`CellPublisher` fixture from
   `tests/runtime/publication.rs`. Name the seams: after SQLite commit and
   before capture; after capture and before follower proof; after follower
   fsync and before object CAS; after accepted CAS with its response lost;
   after object proof and before cut pruning; owner loss during takeover.
   Inject one failure at a time with the existing host/store/transport hooks.
2. For each seam, record the request identity, committed sequence, LTX
   position, selected follower tickets, root digest, and observed control.
   Reopen from the authority-pinned root plus any sealed tail; replay the same
   request identity. Assert that a success has recoverable bytes, no second
   application effect, monotone sequence, and no stale-owner output. For an
   unknown outcome, assert that reconciliation resolves it without rerunning
   SQL blindly. A torn or missing object must never become a serving root.
3. Keep the fault schedule and seed in a failure message. Retain every found
   regression as a named deterministic test; add the corresponding abstract
   event to `src/coordination/sim.rs` only if its production decision differs
   from an existing event. Do not duplicate a second coordination state
   machine in the test harness.

**Verify**: `cargo test -p crab-cell-runtime --test runtime publication --locked`
and `cargo test -p crab-cell-runtime --test runtime durability --locked`, with
the external target above, exit 0. Then run the runtime, LTX, and layout gates.
**Done**: every named seam has a real-path test and a recorded expected
outcome; the simulator and real path make the same admission/fence decision.

Current real-path evidence: `capture_failure_after_sql_commit_fences_until_authoritative_recovery`
proves an uncaptured commit stays unknown and restores as absent from the pinned
root. `object_root_publication_fences_actor_when_local_pruning_fails` and
`published_root_survives_local_prune_failure_without_replaying_sql` prove that
an accepted root survives failed local cleanup and replay does not rerun the
handler. Existing `lost_publication_response_reconciles_without_replaying_sql`,
`fleet_proof_retains_owner_when_object_publication_fails_first`, and
`source_loss_takeover_restores_exact_root_and_continues_publication` cover
the named CAS, follower/object, and owner-loss branches. The fleet/object
test now delays the selected follower after object failure, proves no early
answer, then uses `LocalFollowerTransport` and asserts its fsynced receipt for
sequence one. `accepted_control_cas_with_lost_response_releases_once` now
covers the same CAS ambiguity through the production actor and restores the
recorded result. `missing_authoritative_root_cannot_become_a_serving_cell` and
`torn_authoritative_root_cannot_become_a_serving_cell` prove a fresh receiver
cannot serve when the pinned root object is missing or corrupt.
`crashed_process_is_fenced_before_successor_restore` now carries a committed
request across a child-process crash: the successor reads its changed SQL bytes
and resolves the original request as committed.

| Fault boundary | Deterministic real-path test | Observed decision |
| --- | --- | --- |
| SQLite committed, capture file creation fails | `capture_failure_after_sql_commit_fences_until_authoritative_recovery` | Unknown; pinned root unchanged; restore resolves absent |
| Captured cut, object publication fails before follower receipt | `fleet_proof_retains_owner_when_object_publication_fails_first` | Wait for the selected follower's fsynced sequence-one receipt; only then answer |
| Follower fsync completes while object root CAS is paused | `follower_fsync_can_acknowledge_before_object_root_cas` | Return committed result from the follower proof with the root still at sequence zero; finish CAS, replay once, and restore exact sequence-one bytes |
| Follower fsync completes but its acknowledgement is lost; object publication also fails | `lost_ack_suffix_recovers_an_ambiguous_command_without_reexecution` | Return unknown; successor seals the retained suffix, restores sequence two, and replays the recorded outcome |
| Control CAS accepted, response lost | `accepted_control_cas_with_lost_response_releases_once` | Reconcile exact root and return recorded result once |
| Root accepted, local cut prune fails | `object_root_publication_fences_actor_when_local_pruning_fails` | Unknown and fenced; pinned root restores committed bytes |
| Owner process crashes during transfer | `crashed_process_is_fenced_before_successor_restore` | Successor restores sequence one and resolves recorded request |
| Pinned root missing or corrupt on fresh receiver | `missing_authoritative_root_cannot_become_a_serving_cell`, `torn_authoritative_root_cannot_become_a_serving_cell` | Acquisition fails; control returns Idle; no serving Cell |

These tests use fixed request IDs. The follower/object test injects object
failure **before** the selected follower fsync. The paused-CAS test isolates
the post-fsync/pre-CAS window with a real `LocalFollowerTransport` and no
second failure. The lost-ack suffix test injects follower response loss
**after** its local fsync alongside failed object publication.
A fault log now records the request, committed sequence, selected member and
authenticated node-frame range, fsynced receipt, and observed control in both
follower/object fault tests. The capture-failure test reads `sys_requests` from the
local SQLite file to prove sequence one committed before capture failed; its
fault log records that sequence alongside the still-sequence-zero control root.
The lost-ack suffix test now records the selected follower's authenticated
sequence-two frame and fsynced receipt after its reply is dropped, together
with the sequence-one authority root and the ambiguous request identity.
The source-loss, child-process crash, and missing/torn-root tests now print
their exact root position, digest, committed sequence, and observed control at
the fault point. The root corruption tests have no request or follower ticket
because acquisition fails before serving or executing a command.
The simulator now has a distinct accepted-root/prune-failure event because a
successful root CAS followed by failed local cleanup fences the actor and
returns unknown to an unanswered caller. Its deterministic tests check that
decision and preserve an earlier follower acknowledgement; the broad seed and
bounded exhaustive runs include the new event.
The other seams use existing pure-kernel decisions: `Fence` for failed capture,
`FollowerProof` and publication completion for the two durability winners,
`ExactCasAfterLostResponse` for ambiguous CAS, and crash/restart/movement
events for ownership loss. A missing or torn root is rejected during physical
recovery before a kernel acquisition decision. Fixed request IDs and
`fault_seed`/`schedule` diagnostics retain reproducible failure cases.
The full runtime `test-support` suite and LTX `replica` suite pass locally;
protected CI replay against a candidate source is still required for the
delivery gate.
The current full run passed 356 runtime unit tests, 128 runtime integration
tests, all qualification and other integration suites, plus 82 LTX unit, 87
LTX integration, and five LTX doc tests. One runtime unit, two runtime
integration, and five LTX doc tests remained ignored; the ignored RustFS
cases require a separate provider environment. Runtime and LTX all-targets
Clippy passed with `-D warnings`, as did format, layout, docs validation, and
diff whitespace checks.

## Slice 2 — Establish an honest per-Cell resource envelope

**Owner and scope**: `src/fleet/resource.rs`, `src/cell/worker.rs`,
`tests/runtime/lifecycle/{residency,idle/churn}.rs`, the existing qualification
runner and `crates/crab-http-server` capacity projection. Preserve one shared
ledger; do not add a second resource counter or a new tuning environment
variable.

1. In the protected scale harness from plan 015, measure 1,000, 5,000, and
   10,000 **genuinely open** Cells on Linux. Record before/after and peak RSS,
   allocator, threads, descriptors, SQLite/cache bytes, active/retained/local
   disk reservations, and per-Cell slope. Include empty, sparse, resident,
   pending-publication, and churned Cells. Emit raw measurements as artifacts
   bound to source/image/profile; a test that only constructs `ResourceCost`
   values is insufficient.
2. Compare observed slope and fixed overhead with the 64 KiB/eight-descriptor
   admission constants. If either undercharges, change the constant or its
   actual allocation at the owner and prove the resulting node capacity report
   never advertises more than the measured envelope. Check actor, worker,
   `crab-ltx::Db`, and server projection together. Do not reduce a constant
   solely to improve a capacity headline.
3. Run repeated activation → publication → eviction → process restart cycles
   on a bounded node. Prove the ledger equals the filesystem and returns to a
   stable baseline after cleanup. The existing startup inventory is a safe
   quarantine, not reclaim. If stale session deletion is necessary, first
   prove process-wide exclusive ownership, no live handle or follower/pin
   reference, and exact authoritative recovery; then implement deletion in
   `crates/crab-http-server/src/local_disk.rs` with symlink/special-file
   refusals and interruption-safe restart tests. If that ownership proof is
   unavailable, leave the bytes charged and report the capacity ceiling.

**Verify**: focused `cargo test -p crab-cell-runtime --test runtime lifecycle`
plus `cargo test -p crab-http-server --locked --lib local_disk` when changed;
protected scale receipt passes the unchanged `scale-v1` profile. Also run
runtime, LTX, and receipt-contract gates. **Done**: the receipt contains all
five workload states and measured slopes, admission constants are supported
by those measurements, and restart loops neither undercharge nor accumulate
unbounded reclaimable residue.

Local startup evidence: `restart_inventory_counts_stale_sessions_but_not_the_new_session`,
`restart_inventory_accounts_for_cell_recovery_and_compaction_scratch`, and
`restart_inventory_fails_closed_when_stale_bytes_exceed_capacity` prove
conservative accounting of one prior session. `staging_holds_restart_inventory_until_the_owner_drops`
proves the reservation stays live with the staging owner.
`repeated_restarts_charge_every_unreclaimed_session_until_capacity_is_exhausted`
proves three successive local starts charge all prior bytes, and the fourth
has no available capacity after 17+17+16 bytes accumulate against a 50-byte
budget; a new staging request is refused. This is an
accounting check, not a measured process/Cell restart loop, and it does not
prove that stale sessions can be deleted. The
protected restart receipt must report accumulated quarantined bytes and
resulting usable capacity; release of the in-memory reservation at process
exit is not on-disk reclamation.
Server startup now warns with prior-session count, charged bytes, remaining
budget, and budget capacity when an older session directory exists. This makes
the conservative capacity ceiling observable without reclaiming files whose
ownership has not been proved.
The `scale-v1` signed primitives row now requires fifteen raw resource samples
covering 1k/5k/10k actually open Cells in each of the five required workload
states. Its signer and fresh-process verifier reject missing, duplicate,
false-open, undercharged steady-state, and underreported-peak artifacts. This is a receipt contract;
the protected harness still has to produce real Linux samples and demonstrate
that the 64 KiB/eight-descriptor admission costs cover observed slopes.
The local scale contract test exercises the signer and receipt verifier with
undercharged memory, SQLite cache, and descriptor slopes. The memory bound
uses the configured 192 KiB page-cache reservation per added Cell plus native
and retained charges; it does not charge fixed process overhead to every Cell.
Qualification unit and integration tests, runtime Clippy, format, layout, and
docs checks pass after this gate.

## Slice 3 — Bound hot-Cell and many-Cell cost from measurements

**Depends on**: plan 034's pending throughput/resume slices and Slice 2's
resource envelope. **Owner and scope**: `src/cell/actor/`,
`src/cell/executor.rs`, `src/publication.rs`, `src/fleet/telemetry.rs`, the
existing server metrics adapter, and the canonical qualification workload.

1. Run the tracked scale workload with one hot Cell plus a many-Cell mix,
   separately with object proof and enrolled follower proof. Record command
   p50/p95/p99, proof source, actor queue wait, SQL/capture time, publication
   queue depth and bytes, root lag, immutable object requests and bytes per
   command, compaction debt, and scheduler due lag. Include a 60-minute steady
   run and a burst that reaches admission bounds. Use the existing telemetry
   and `CellReplica::take_publication_cost`; add a finite-label metric only
   where a phase is otherwise invisible. Never put raw Cell IDs in Prometheus
   labels.
2. Attribute the failing p99 or capacity bound to one measured phase before
   editing code. Make one bounded optimization at that owner, with a paired
   before/after run on the same source-equivalent workload, provider, node
   profile, and proof mode. Preserve predecessor CAS ordering, retained cuts,
   and the 64-cut/64-MiB admission behavior unless the protocol proof and
   tracked profile explicitly justify a change.
3. Add a focused regression for the observed failure mode, not a test that
   mirrors the implementation. If no tracked bound fails, record the measured
   headroom and close this slice without speculative caching or batching.

**Verify**: runtime and LTX gates; the same protected profile validates both
receipts. **Done**: the run reports the listed phase/cost fields, no invariant
regresses, and either a measured failing bound is fixed or the current
headroom is recorded with no code change.

Local instrumentation map: finite durability-proof source and wait, LTX
capture phases, immutable publication object/byte cost, and scheduler lag
already reach the server metrics adapter. Publication-start traces record
actor queue wait, queued count/bytes, and the signed sequence lag from the
last authoritative object root; preparation traces report compaction debt.
Protected hot/many-Cell traces and paired receipts remain open.

## Slice 4 — Qualify hydration, takeover, and offline retention together

**Depends on**: plans 009, 015, and 032. **Owner and scope**:
`tests/runtime/lifecycle/residency.rs`, `src/recovery/{backup,retention}.rs`,
their tests, `crab-ltx` sparse/root tests, and the existing provider harness.

1. For 100 MiB, 1 GiB, and 5 GiB Cells, measure sparse activation,
   background hydration, verified promotion, and fully resident queries at
   the provider-latency profiles already defined in
   `docs/canonical-ltx-scaling.md`. Count origin requests from route to SQL
   result; a promoted resident read must make zero object-store calls.
2. Inject origin failure, cancellation, owner loss, local-disk exhaustion,
   and shutdown during hydration. Assert no unauthenticated page is served;
   each failure either retries safely, fences, or remains sparse according to
   the existing actor contract. Check every reservation after teardown.
3. Enter the **offline** maintenance fence and use the canonical collector.
   Interleave its mark/sweep with *prepared but unpublished* objects from a
   previous serving epoch, backup pins, and representation-only compaction;
   verify every current/pinned root reopens and obsolete objects alone are
   eligible after grace. Do not run collection concurrently with a serving
   owner. Use the existing `src/recovery/retention/tests.rs` as the pattern.

**Verify**: focused runtime residency/retention tests, LTX sparse/root tests,
then the provider and fault receipts under plan 015. **Done**: recorded
promotion percentiles and failure outcomes, zero origin calls after verified
promotion, and exact reopen of every retained root after collection.

Local fault evidence: `origin_failure_during_hydration_fences_without_serving_unverified_pages`
pauses a real background origin read, injects a non-retryable provider denial,
then proves the Cell stops serving, the hydration reservation clears, the
authority-pinned root reopens exactly, and local disk admission returns to
zero. `disk_exhaustion_during_hydration_fences_and_releases_capacity` bounds
the successor's local disk below the authenticated database size, then proves
the Cell fences, its hydration job and reservation clear, and the pinned root
reopens exactly. The existing shutdown and resident-route tests cover
cancellation and zero-origin promotion.
`node_lease_loss_during_hydration_cannot_promote_a_stale_owner` pauses the
origin fetch, terminally fences the node lease, then proves the route and
retained handle cannot serve while the root still reopens and disk admission
returns to zero.
`maintenance_collection_preserves_live_and_pinned_graphs`
now restores the compacted current root, its previous representation retained
by a backup pin, and another pinned root after the offline sweep, reading each
exact original payload length. The compaction changes the root digest while
preserving its position and commit sequence. The test also prepares an
unpublished root for a different incarnation of the current Cell and proves
that only its unreferenced objects are deleted after grace. A real successor
takeover now runs while the predecessor's origin read remains blocked:
`successor_takeover_while_hydration_waits_keeps_the_old_owner_fenced` proves
the successor reads the exact 8 MiB payload under the pinned root, then the
old hydration finishes without serving or retaining disk capacity. Large-Cell
latency and provider receipts remain open.

## Slice 5 — Prove the framework boundary with another application

**Depends on**: Slice 1 and the protected primitive work in plan 024.
**Owner and scope**: `crates/crab-cell-app`, `crates/crab-cell-host`, their
examples/tests, and narrow `crab-cell-runtime` public APIs proven necessary.
Do not move repository HTTP/auth or provider construction into the runtime.

1. Drive the existing full-primitive reference application through
   `CellApplication`/`CompiledApplication` and `CellNode` on at least two
   distinct Cell types and several partitions. Use typed commands, queries,
   effects, and an idempotent activity. Capture exact outputs before and after
   owner loss and source-loss restore. This is the reusable framework test;
   repository-specific routes are a separate product test.
2. Audit every API the example needs: stable identity and descriptor bytes,
   schema migration, scoped capability, application-level admission, error
   resolution, shutdown, and release compatibility. Put mechanics at the
   existing owner. A proposed new root export must update `api-prelude.txt`
   and prove its direct consumers. Avoid aliases, broad fallback readers,
   or exposing `CellAuthority`/`CellReplica` to application authors.
3. Extend the canonical protected primitive receipt only for an unproven
   behavior. Bind the test to source/image and raw provider evidence. Do not
   call an in-memory or local RustFS run a production provider receipt.

**Verify**: `cargo test -p crab-cell-app -p crab-cell-host -p crab-cell-runtime
--locked` with the external target, layout/API checks, and the unchanged
protected primitive profile. **Done**: another application exercises the
same public boundary through source-loss recovery without a product-specific
runtime branch.

Local framework evidence: `public_cell_node_primitive_takeover_preserves_acknowledged_values`
restores seven Cells across six typed modules, including two distinct SQL
partitions, from an empty successor directory. It compares acknowledged SQL,
KV, blob, queue, cron, workflow, activity, and effect results, then proves
stale-owner writes fail on both SQL partitions. The shared
`public_cell_qualification` suite also passes with the two-shard descriptor.
The source/image-bound protected primitive receipt remains open.

API audit so far: `CellType` validates stable shard counts and
`CompiledApplication` emits deterministic descriptor bytes; `Registry` owns
ordered migrations and rolling-compatibility checks. `ApplicationHandle`
checks tenant/application/partition scope and resolves ambiguous mutations.
`CellNode` owns admission status and ordered drain/shutdown. The operator API
audit is complete: `CellNode::runtime` exposes the same admission-controlled
runtime used by product initialization; the reference fixture provisions with
`CellCatalog`/`CellAuthority` and activates through `CellRuntime::bootstrap`,
then restores with `CellRuntime::takeover_restored`. The product initializer
uses `CellNodeBuilder`, `CellRuntime::bootstrap`/`acquire_idle_restored`, and
`ReleaseStore::provision` to bind catalog entries to its selected release.
The direct `CellCatalog::provision` in the reference test deliberately omits
product release policy; it does not require a second runtime path. Catalog,
authority, replica, and release objects remain operator inputs outside
`crab-cell-app`'s author API. No runtime root export or `api-prelude.txt`
change is needed (`crates/crab-http-server/tests/support/reference_application.rs`,
`crates/crab-http-server/tests/public_cell_takeover.rs`,
`crates/crab-http-server/src/cells/initializer.rs`,
`crates/crab-cell-runtime/src/recovery/release.rs`).

The audit found that `CellType::with_limits` had only encoded database and
capture ceilings in descriptor bytes. `CellNodeBuilder` now installs those
ceilings in its runtime; every bootstrap and restored acquisition checks the
supplied `CellReplica` limits before an ownership transition or root open.
Restored paths also check the independently supplied recovery manifest store.
`node_public_application_handle_executes_typed_sql` rejects a replica whose
ceilings differ from the descriptor, verifies control remains rootless, then
serves through the correctly bounded replica. The shared reference fixture
now uses its declared ceilings on source and successor nodes.
`public_cell_node_primitive_takeover_preserves_acknowledged_values` rejects
a recovery store with mismatched ceilings before takeover and then restores
the exact root through the correctly bounded store.

## Slice 6 — Make regression evidence a continuing release gate

**Depends on**: Slices 1–5. **Owner and scope**: existing
`.github/workflows/cell-runtime-qualification-contract.yml`,
`cell-coordination-model.yml`, `cell-runtime-protected-qualification.yml`,
`crab-ltx-fuzz.yml`, `http-server-release.yml`, and the canonical receipt
validator. Do not create a parallel signer, profile format, or CI harness.

1. Keep fast PR checks for the deterministic fault corpus, both runtime and
   LTX feature sets, layout/API guards, and profile/receipt validation. Keep
   deep fuzz/model search and sustained resource/performance runs in scheduled
   or protected jobs with retained seeds, traces, raw metrics, and first
   failure artifacts. A changed runtime protocol must update both the real
   path test and model/simulator coverage in its PR.
2. Require a fresh protected bundle for any source/image promoted as a Cell
   production release. Use `qualification_receipt verify-protected-bundle`
   and the exact tracked profile digests. A missing provider, fault, scale,
   compatibility, or application row fails closed. Never manufacture receipts
   from a local run or relax a threshold without a separate reviewed decision.
3. After each protected run, classify regressions as correctness, resource,
   latency, provider, or evidence failure and link the raw artifact to the
   owning plan. The next run must replay retained failure seeds. Review the
   resource slopes and p99 attribution on every release; update constants only
   with new measurement and direct-consumer proof.

**Verify**: workflow syntax/contract job passes; the receipt verifier rejects
a missing row, mismatched source/image, stale receipt, altered profile, and
unsigned artifact in its existing qualification tests. The protected job
passes with fresh real evidence. **Done**: no release can pass on local-only
evidence, and every failed run has a replayable artifact and named owner.

The release workflow previously created immutable production image tags before
checking the protected bundle. It now binds local and protected evidence to
the candidate digest, verifies the protected bundle, packages it, and only
then promotes that candidate to versioned tags. The qualification contract job
checks this order so a failed receipt cannot leave a published release tag or
block a clean retry. A live protected run is still required.
The tag-push run now retains its exact candidate reference, digest, and source
as an artifact and stops before promotion. After manual protected qualification
of that digest, the release dispatch takes the candidate run ID and protected
run ID, verifies both handoffs, repeats Compose on the same digest, and then
promotes it. This closes the prior timing gap in which a new release run rebuilt
a run-specific candidate that could differ from the protected image.
The scheduled simulator previously used a stale `--exact` test path and ran
zero tests with exit status zero. Its job now names the real test, requires its
success line, prints each attempted seed, and saves a source-bound replay log.
Both simulator and TLC
`tee` pipelines now propagate the underlying failure. Nightly LTX fuzzing
retains source/target/time context and each decoder log alongside crash inputs;
a decoder failure stops the matrix and uploads the first failure evidence.
The actual simulator step ran one test locally. Simulated TLC and fuzz command
failures produced nonzero step status with retained logs. These checks do not
replace a live scheduled CI run.

## Program completion and stop conditions

| Gate | Completion evidence |
| --- | --- |
| Safety | Real-path fault tests and simulator seeds; no false success, stale owner, root rewind, torn object, or leaked obligation |
| Capacity | Signed 1k/5k/10k measurements match ledger admission; restart churn returns or conservatively charges capacity |
| Performance | Protected scale profile and hot-Cell cost/latency breakdown pass without a profile edit |
| Recovery | Sparse promotion, source-loss takeover, and fenced offline retention reopen exact roots |
| Reuse | Independent typed application runs through the public Cell framework and survives owner loss |
| Release | Fresh, complete protected bundle tied to exact source, image, provider, and profile |

Stop the affected slice and report the first reproducible counterexample if:

- an acknowledged result cannot be reconstructed from the exact root or
  selected follower proof;
- a cleanup path cannot prove exclusive ownership of stale bytes;
- a proposed optimization needs a second publication protocol, mutable head,
  unstated compatibility path, new config switch, or changed profile threshold;
- the provider/scale environment or trusted signer is unavailable. Local tests
  may continue, but the protected gate stays open;
- a prerequisite plan's source or contract has drifted. Reconcile that plan
  and its direct consumers before continuing.

For every implementation slice, inspect the whole changed function/module,
its caller and callee, sibling surfaces, adjacent tests, and current `main`.
Ask whether the change is the best owner-level fix. Keep docs with behavior,
and include `git diff --numstat` when a nontrivial refactor grows production
code. Do not mark this program DONE while any table gate is missing.
