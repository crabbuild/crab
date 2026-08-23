# Plan 001: Close the current writer/GC fence

> **Executor instructions**: Follow this plan in order. Run every verification
> command and record its result before moving on. If a STOP condition occurs,
> stop and report; do not invent a compatibility path. When complete, update
> the status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat b738f3b2..HEAD -- crates/crab-coordination/src crates/crab-storage/src/layout.rs crates/crab-metadata/src/ref_registry.rs crates/crab-auth-server/src/receive crab/src/maintenance.rs crab/src/git/push.rs crab/src/cmd/gc crab/src/cmd/repack.rs crab/src/cmd/restripe.rs crab/src/restripe crab/src/cmd/recover.rs crab/src/cmd/history_recovery.rs crab/src/replication crates/crab-workflow/src`
> If any listed writer or GC namespace changed, rebuild the writer map before
> editing. A mismatch in any excerpt below is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `b738f3b2`, 2026-08-22

## Delivery status

Implemented in this branch: the current writer paths use shared global/repo
GC fences and destructive repo/bucket GC uses renewable exclusive sweep fences.
The complete writer inventory and injected race qualification remain release
gates.

## Why this matters

Destructive GC currently holds only `repository-maintenance`. Direct pushes use
per-ref locks plus weighted push-admission slots, repack uses `repack`, and
restripe still probes a local `gc.lock`. Those protocols do not make a remote
object uploaded before publication visible to GC, so a long writer or any
force-mode sweep can delete a live-but-not-yet-rooted object. This plan adds one
object-store-backed shared/exclusive fence at the existing writer boundaries,
without blocking an entire long GC run or moving protected/active-active
ownership into the client.

## Current state

- `crab/src/cmd/gc/mod.rs:1078-1160` acquires and renews
  `REPOSITORY_MAINTENANCE_RESOURCE` only for destructive repo GC; bucket GC
  does the same for every registry repo (`crab/src/cmd/gc/bucket.rs:127-168`).
- `crab/src/maintenance.rs:8-48` wraps a `PushLock` heartbeat. It has no
  shared writer side and no global bucket domain.
- Direct push admission is already real and must be reused, not duplicated:
  `crab/src/git/push.rs:15179-15235` acquires it after the ref lock and
  `crab/src/git/push.rs:5471-5520` renews/releases it through canonical commit.
  `crab/src/git/push.rs:3193-3197` intentionally excludes protected and
  active-active pushes from this object-store ticket.
- The ticket implementation is a bounded weighted slot set under the repo
  prefix (`crates/crab-coordination/src/push_admission.rs:18-340`). It has no
  exclusive sweep state and cannot represent a bucket-global domain.
- Protected receive is service-owned. The receive path publishes canonical
  objects and acquires `GIT_OBJECT_LOCATOR_RESOURCE`
  (`crates/crab-auth-server/src/receive.rs:1340-1410`); the client must not
  acquire a lock against a session-private store and call that proof complete.
- Active-active GC reads only coordinator-owned protected keys
  (`crab/src/replication/mod.rs:7084-7139`), while the coordinator exposes a
  mutable `fence_writes` API and an epoch
  (`crates/crab-coordination/src/write_coordinator.rs:982-1024`). A key list
  without an epoch/fence is not a deletion proof.
- Repack holds only `REPACK_RESOURCE` (`crab/src/cmd/repack.rs:123-141`), and
  restripe's `check_gc_not_running` checks only `<repo>/.crab/gc.lock`
  (`crab/src/restripe/executor.rs:545-551`, called by
  `crab/src/cmd/restripe.rs:450`). Neither protects a remote GC process.
- Current production writes to GC-managed namespaces include direct push,
  protected receive, active-active materialization, repack, history/recovery
  restore, restripe executor/reconcile, replica propagation, and workflow
  cache/artifact publish. Confirm each with `rg` before coding; test fixtures
  that call `put` directly are not writer paths.

The protocol must preserve these current invariants:

1. A writer owns shared admission before its first canonical upload or adoption
   and until its visible root (manifest, ref-registry, coordinator commit,
   workflow ref, or replica publication) is durable.
2. A sweep owns exclusive admission only for one bounded mark/revalidate/delete
   batch. It never relies on a client wall clock to declare a writer dead.
3. Repo and bucket domains use canonical paths supplied by `StoreLayout`; no
   hand-built `.crab` or prefix strings in callers.
4. An expired/crashed writer remains quarantined until its lease evidence and
   the configured grace policy are both satisfied. `--force` never bypasses
   that quarantine.
5. Active-active GC fences the coordinator transaction authority and records
   the returned epoch; a protected-key snapshot taken before fencing is not
   sufficient.

## Commands you will need

Run Cargo commands only after confirming the external target directory is
writable; never create a local `target/` in this worktree.

| Purpose | Command | Expected on success |
|---|---|---|
| Writer inventory | `rg -n "(xorb_path|shard_path|pack_path|workflow/artifacts|refs/crab/artifacts|REPACK_RESOURCE|GIT_GENERATION_OWNER_RESOURCE|REPOSITORY_MAINTENANCE_RESOURCE)" crab crates -g '*.rs'` | every production writer is classified in the map |
| Coordination tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-coordination --locked push_admission` | existing and new admission tests pass |
| Metadata tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata --locked ref_registry` | registry CAS and union invariants pass |
| GC tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cmd::gc` | all current GC tests pass |
| Writer tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked push_admission` | direct push admission/race tests pass |
| Service tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-auth-server --locked receive` | protected receive tests pass |
| Formatting | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope** (modify only these implementation surfaces and their focused
tests/docs):

- `crates/crab-coordination/src/push_admission.rs`, `push_lock.rs`, `lib.rs`,
  and coordination errors/tests (canonical shared/exclusive fence)
- `crates/crab-storage/src/layout.rs` (repo/global fence paths)
- `crab/src/maintenance.rs`, `crab/src/cmd/gc/{mod,bucket}.rs`, and
  `crab/src/main.rs` (sweep acquisition and policy)
- `crab/src/git/push.rs` and direct-push tests
- `crates/crab-auth-server/src/receive/` and protected-receive tests
- `crab/src/replication/mod.rs` and coordinator adapters/tests
- `crab/src/cmd/repack.rs`, `crab/src/cmd/history_recovery.rs`,
  `crab/src/cmd/recover.rs`, `crab/src/cmd/restripe.rs`, `crab/src/restripe/`,
  and the remote-writing
  workflow modules identified by the writer map
- current GC/recovery/architecture documentation and CLI/config tests

**Out of scope**:

- PB layout, recipe, partition, cache-service, or migration assumptions
- replacing the current manifest/ref-registry/history roots
- the bounded durable run engine (Plan 002), closure sidecars (Plan 003), or
  provider inventory readers (Plan 004)
- making all pushes wait behind all of GC; admission must be shared except for
  a bounded sweep fence
- using local `gc.lock` as a remote safety contract

## Git workflow

- Branch: `codex/gc-hardening-roadmap` is the plan-delivery branch; the
  executor should use its normal feature branch convention from the Crab repo.
- Keep commits grouped as protocol, writer integration, sweep integration, and
  policy/docs. Do not push without operator instruction.

## Steps

### Step 1: Freeze and test the complete writer map

Search production code (excluding `#[cfg(test)]` blocks) for every write to
repo-local GC prefixes, global `xorbs/` and `shards/`, `.crab/ref-registry`,
workflow artifacts, and replica destinations. Record the owner boundary,
domain(s), existing lease, publication point, and cancellation path in a
table-driven coordination test. Include the paths listed in Current state and
any additional caller discovered by `rg`; a writer omitted from the table is a
release blocker. Mark read-only cache/download paths explicitly so they are not
mistaken for writers.

**Verify**: the writer-map test enumerates every production owner and has no
`unclassified` row; the inventory command above and
`cargo test -p crab-coordination --locked writer_map` both pass.

### Step 2: Add one canonical shared/exclusive fence over current admission

Implement a bounded `GcWriterLease`/`GcSweepLease` protocol in
`crates/crab-coordination` using the same conditional object-store primitives
and backend-time semantics as `PushLock` and `PushAdmissionTicket`. Do not add
a second uncoordinated capacity counter. The protocol may extend the existing
ticket state, but it must expose explicit repo and bucket domains and an
exclusive sweep marker. `StoreLayout` supplies canonical path builders.

The persisted state must contain a schema version, domain identity, monotonic
epoch, fixed-size writer slots/holder fingerprints, optional sweep holder, and
quarantine records bounded by a documented maximum. CAS transitions must prove
the holder and expected version. Lease renewal failures cancel the child
operation; release is holder-checked and idempotent. Acquisition order is
global bucket then repo; release is repo then global. A sweep cannot acquire
while a live writer exists, and a writer cannot acquire while a sweep exists.

Add model/property tests for stale CAS, expiry, cancellation, process death,
epoch monotonicity, writer/sweep exclusion, bounded holder state, and retry
idempotence. Keep existing `PushAdmissionTicket` weighting for direct push
back-pressure; make its writer guard own the shared fence rather than making
GC guess from slot occupancy.

**Verify**: `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main
cargo test -p crab-coordination --locked gc_fence` passes all state-machine,
CAS, heartbeat, and in-memory object-store tests.

### Step 3: Integrate every writer at its orchestration boundary

Acquire the shared writer lease before the first canonical object write, not at
individual `put` calls. Hold it through the publication boundary and release
only after that boundary succeeds:

- Direct push wraps the existing admission at the point shown in
  `crab/src/git/push.rs:15179-15235`; it covers global xorb/shard uploads and
  repo packs/metadata, and remains held through manifest/ref-registry CAS.
- Protected receive acquires the service-owned global/repo lease before
  promoting session objects and keeps it through service manifest publication.
  The client-side protected path remains admission-free as today.
- Active-active commit/materialization carries the fence epoch into the
  coordinator request and rejects stale epochs. GC uses a new scoped
  coordinator fence/lease that drains or records in-flight transactions before
  a delete batch; do not treat `gc_safety_snapshot().protected_keys()` alone as
  sufficient.
- Repack, history restore/prune, recovery apply, restripe executor/reconcile,
  replica propagation, and workflow cache/artifact publish acquire the correct
  repo/global domain. Keep their existing locks for their own invariants, but
  never use them as a substitute for the GC fence.

Every writer path must close/release on success, cancellation, and error. A
writer that loses the lease must stop before its next upload or publication and
return a structured busy/lease-lost error; it must not continue under grace.

**Verify**: focused tests for each inventoried writer show a sweep blocks before
the first canonical write, a writer blocks a sweep, lease loss cancels work,
and all guards release after an injected error. Run the writer, service,
metadata, and coordination commands in the table.

### Step 4: Fence each current delete batch and revalidate roots

Retain the existing maintenance lease where history/recovery needs it, but add
the exclusive fence around each existing bounded delete chunk. For repo GC,
re-read the manifest ETag/generation, active ref-journal frontier, historical
manifest digest, workflow/artifact roots, and generation-owner status before
deleting. For bucket GC, re-read the registry ETag/generation, every affected
repo root, closure-coverage marker (when Plan 003 is present), and coordinator
epoch/protected set. If any snapshot differs, discard the batch and re-mark.

Force mode still prompts consistently for both scopes, but the fence, root
revalidation, registry completeness, coordinator proof, and quarantine checks
always run. Ensure `cleanup_orphaned_bulk_objects` and bucket `delete_or_report`
use the same guarded path; no direct delete helper may bypass it.

**Verify**: deterministic barrier tests cover writer-before-mark,
writer-during-mark, writer-between-batches, root drift, coordinator epoch
drift, sweep renewal loss, cancellation, and `--force`. No guarded test deletes
an object that becomes reachable before its batch commits.

### Step 5: Make grace and force policy one current contract

Remove the independent Clap `1h` default and resolve an omitted CLI value from
`Config::gc_grace_period` (current default 24h). Add the same confirmation
behavior to bucket scope; `--yes` skips only the prompt. Keep the existing
one-hour minimum clamp where non-force code requires it, but document that
current `--force` bypasses age entirely. Do not add a new environment variable
until existing config overlay and doctor/configure flows are shown insufficient.

Update `crab/docs/guides/gc.md`, the architecture/storage docs, and both web GC
pages to describe remote writer fencing, current/history/workflow roots,
coordinator fencing, and the exact CLI contract. Add parser/config tests for
repo and bucket scope, JSON/JSONL output, omitted grace, explicit override,
force prompt, and non-interactive `--yes`.

**Verify**: CLI/config tests pass, `crab gc --help` shows one default contract,
the docs contain no contradictory `1h`/`24h` safety claim, and the GC regression
suite plus format check pass.

## Test plan

- Pure fence state-machine/property tests and conditional object-store tests.
- Direct push, protected receive, active-active coordinator, repack, recovery,
  history, restripe, replica, and workflow writer characterization tests.
- Barrier-controlled race tests at upload, registry/manifest CAS, coordinator
  commit, batch mark, revalidation, delete, cancellation, and release.
- Process-death/expired-holder tests proving quarantine and later safe cleanup.
- CLI/config/JSON/JSONL tests proving one grace/force policy.

Use existing `push_admission`, `maintenance`, `gc`, coordinator, receive, and
restripe test modules as structural patterns. Do not weaken an existing test to
make a new race pass.

## Done criteria

- [ ] The writer-map test has zero unclassified production writers.
- [ ] Shared/exclusive repo and bucket fences are durable, bounded,
      backend-time based, holder-checked, renewable, and cancellation-aware.
- [ ] Every GC-managed writer proves admission through its publication boundary;
      protected and active-active ownership remains service/coordinator-owned.
- [ ] Every destructive delete helper uses a bounded exclusive fence and root
      revalidation; no direct delete bypass remains.
- [ ] Force never bypasses admission, root proof, coordinator epoch, or
      quarantine; both scopes confirm consistently.
- [ ] Focused tests, `cargo test -p crab --lib --locked cmd::gc`, and format check
      pass with `CARGO_TARGET_DIR` on the workspace volume.
- [ ] No files outside Scope are modified and the plan index is updated.

## STOP conditions

- Any production writer cannot be assigned a repo/global domain and publication
  boundary from code evidence.
- The proposed fence requires trusting client wall time, unbounded holder state,
  or a new fallback key/layout.
- Protected receive would need client credentials or a session-private store to
  prove canonical admission.
- Active-active coordinator cannot expose an atomic scoped fence/epoch proof;
  stop rather than relying on a stale protected-key snapshot.
- A writer integration requires changing PB-only layout code or an out-of-scope
  data contract.
- Any focused verification fails twice after a reasonable fix attempt.

## Maintenance notes

Future writer paths must be added to the writer-map test before they can write
GC-managed prefixes. Reviewers should inspect lock ordering, lease-loss
cancellation, publication-after-admission ordering, and coordinator epoch
handling. Plan 002 will move batch state into a durable run engine but must keep
this fence API and its invariants; Plan 003 must acquire it for closure
backfills. Never reintroduce the local `gc.lock` as a remote safety mechanism.
