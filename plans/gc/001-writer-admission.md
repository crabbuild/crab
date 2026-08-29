# Plan 001: Prevent GC from racing any current remote writer

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If a STOP condition occurs, stop and report; do not improvise.
> When done, update this plan's status row in `plans/gc/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat ff7792a4..HEAD -- crates/crab-coordination/src crates/crab-storage/src/layout.rs crates/crab-metadata/src/ref_registry.rs crates/crab-auth-server/src/receive crab/src/maintenance.rs crab/src/git/push.rs crab/src/cmd/gc crab/src/cmd/repack.rs crab/src/import crab/src/restripe crates/crab-workflow/src packages/web/content/docs/cli`
> If an in-scope writer or collected namespace changed, rebuild the writer map
> in Step 1 before proceeding.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: none
- **Category**: security, bug
- **Planned at**: commit `ff7792a4`, 2026-08-22

## Why this matters

Current GC protects recent uploads with age and serializes against recovery,
but ordinary pushes and repack use different locks. A long-running push, or any
force collection, can therefore expose an uploaded-but-unpublished object to
deletion. This plan makes every writer and collector participate in one durable
admission protocol while limiting exclusive pauses to bounded delete batches.

## Current state

- `crab/src/cmd/gc/mod.rs:39-45` collects five repo-local namespaces:

  ```rust
  const REPO_GC_PREFIXES: &[&str] = &[
      "packs/", "metadata/", "manifests/",
      "workflow/artifacts/", "refs/crab/artifacts/",
  ];
  ```

- `crab/src/maintenance.rs:8-37` gives destructive GC a renewable
  `repository-maintenance` lease, but `crab/src/git/push.rs:4995-5060` acquires
  per-ref or `batch` locks and `crab/src/cmd/repack.rs:123-141` acquires the
  distinct `repack` lock.
- `crab/src/git/push.rs:12043-12242` uploads pack body/index/metadata before
  publication. `crab/src/git/push.rs:7165-7193` conservatively union-registers
  shards before manifest CAS, but repo pack objects have no equivalent
  pre-publication root.
- `crab/src/cmd/gc/bucket.rs:121-327` snapshots the bucket registry and later
  deletes shared shards/xorbs. Registry completeness and active-active proof
  fail closed, but a registry change after the snapshot does not fence the
  delete.
- `crates/crab-metadata/src/ref_registry.rs:113-128` deliberately uses union
  semantics so concurrent writers can retain extra roots. Preserve that
  invariant.
- `crab/docs/architecture/object-storage-layout.md` makes object storage
  authoritative and assigns locks to `crab-coordination`. The new protocol
  belongs there; product orchestration remains in `crab/`.

The required protocol is:

1. A writer takes shared admission for every affected domain before it uploads
   or adopts a GC-managed key. Domains are bucket-global content and one repo.
2. Admission is durable CAS state with a monotonically increasing epoch,
   backend-timestamped renewable writer leases, and at most one sweep holder.
3. Acquisition order is global then repo; release order is repo then global.
4. GC computes candidates without exclusivity. For each bounded batch it takes
   exclusive sweep admission, reloads and compares all root generations, marks
   the batch again, deletes only unchanged candidates, then releases.
5. An expired writer is not immediate garbage: its possible objects remain
   quarantined through the configured grace period. `--force` never bypasses
   admission, root revalidation, or expired-writer quarantine.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Coordination tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-coordination --locked gc_admission` | all matching tests pass |
| Protected writer tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-auth-server --locked gc_admission` | all matching tests pass |
| GC/push tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_admission` | all matching tests pass |
| GC regression set | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib cmd::gc --locked` | all GC tests pass |
| Format | `cargo fmt --all -- --check` | exit 0 |

Before any Cargo command, confirm `/Volumes/Workspace` is mounted and
`/Volumes/Workspace/crabbuild-target/crab-main` is writable. Never fall back to
a local `target/`.

## Scope

**In scope**:

- `crates/crab-coordination/src/gc_admission.rs` (create), `lib.rs`, and errors
- `crates/crab-storage/src/layout.rs`
- `crates/crab-metadata/src/ref_registry.rs`
- `crab/src/maintenance.rs`
- `crab/src/cmd/gc/{mod,bucket}.rs`
- `crab/src/git/push.rs`, `crab/src/git/protected_push.rs`
- `crates/crab-auth-server/src/receive/`
- `crab/src/cmd/repack.rs`, history recovery mutation entry points
- remote mutation entry points under `crab/src/import/`, `crab/src/restripe/`,
  `crab/src/replication/`, and `crates/crab-workflow/src/artifact.rs`
- GC CLI policy in `crab/src/main.rs` and `crab/src/core/config.rs`
- GC safety docs under `crab/docs/` and `packages/web/content/docs/cli/`

**Out of scope**:

- The bounded enumeration/journal engine; standalone GC Plan 002 owns it.
- Provider inventory parsing; standalone GC Plan 004 owns it.
- Serializing an entire push behind an entire GC run.
- Treating cache state as a reachability root.
- Weakening grace, history retention, registry completeness, or coordinator
  proof to regain throughput.

## Git workflow

- Branch: `gc/001-writer-admission`
- Commits: protocol/model, writer integration, collector integration, policy/docs.
- Match concise conventional-ish repository messages. Do not push without
  operator instruction.

## Steps

### Step 1: Freeze the writer/collector protocol and complete the writer map

Add a table-driven namespace classifier in `crates/crab-storage/src/layout.rs`
for exactly the repo and global namespaces GC may delete. Search every
production write to those namespaces and record its owning entry point in a
test fixture next to the classifier. The inventory must include direct push,
protected receive, active-active commit/materialization, repack, history
restore, import publish, restripe, replication repair, workflow artifact
promotion, and any additional live caller found by `rg`.

In `crates/crab-coordination/src/gc_admission.rs`, first implement a pure state
machine with `enter_writer`, `renew_writer`, `leave_writer`, `enter_sweep`, and
`leave_sweep`. State contains a schema version, epoch, optional sweep holder,
and live writer leases. Use backend-authored object modification time to judge
lease expiry, matching `PushLock`; never trust a client clock to make deletion
eligible. Model these invariants with property tests:

- writer and sweep ownership never overlap in one domain;
- stale CAS cannot acquire or release another holder;
- a crashed writer leaves quarantine metadata;
- epoch increases on every ownership transition;
- cancellation/retry cannot manufacture an idle state.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-coordination --locked gc_admission_state`
→ all model/property tests pass.

### Step 2: Persist admission and make lease loss cancel the operation

Add canonical repo and global admission paths through `StoreLayout`; do not
hand-build `.crab` or repo prefixes in callers. Persist the state with
create/CAS semantics and holder-checked renewal/release. Provide RAII-style
writer and sweep guards whose heartbeat cancels the child operation if renewal
or ownership verification fails. Bound state size and reject a new writer
before an unbounded holder list can form; benchmark the current global CAS
hotspot before choosing the bound.

Document the serialized schema, expiry/quarantine rules, and lock order in
`crab/docs/architecture/object-storage-layout.md`. This is a persistent
cross-version coordination contract; no legacy-key probing or fallback path.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-coordination --locked gc_admission_store`
→ create, CAS conflict, heartbeat loss, expiry, cancellation, and release tests
all pass.

### Step 3: Admit every production writer at its orchestration boundary

Acquire shared admission before the first GC-managed upload or before adopting
an existing content-addressed object as a new root. Keep it through manifest,
registry, ref, or coordinator publication; release only after the visible root
is durable. Direct push and protected receive must use the same protocol.
Active-active coordinator publication must carry and validate the admission
epoch rather than inventing a service-only exemption.

Do not scatter acquisition around individual `put` calls. Add one guard to the
highest common orchestration context and thread a proof token into lower-level
mutation helpers. Audit all Step 1 writers. Existing history/recovery leases
remain, but their remote writes also need admission. A writer that cannot
acquire admission waits under the existing operation timeout or returns a
qualified busy error; it must not upload first.

**Verify**:
run both protected and direct `gc_admission` test commands from the command
table → every inventoried writer either proves admission or is explicitly
classified read-only; no test-only bypass exists.

### Step 4: Fence and re-mark each bounded delete batch

Refactor repo and bucket destructive paths to take sweep admission only around
one bounded batch. Under the guard, reload the current manifest ETag/generation,
historical-root digest, workflow/artifact roots, ref-registry ETag/generation,
and active-active safety snapshot as applicable. If any differs from the
candidate snapshot, release the guard and restart marking; do not delete a
stale batch. Re-check grace and expired-writer quarantine under the guard.

For bucket GC, global sweep admission is required and affected repo roots must
be revalidated. Avoid holding hundreds of repo maintenance locks for the whole
enumeration once the admission proof covers writers; retain maintenance
serialization only for recovery/history operations whose own invariant needs
it. Cancellation stops before the next batch and always releases the guard.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_admission_race`
→ deterministic tests cover writer-before-sweep, writer-during-mark,
writer-between-batches, root change before delete, lease loss, cancellation,
and force mode; guarded objects are never deleted.

### Step 5: Resolve grace and force policy once

Make the CLI grace override optional and resolve it over
`Config::gc_grace_period`; remove the current independent `1h` Clap default so
the configured/default 24-hour policy is actually used. Apply one policy type
to repo and bucket scopes. Both scopes require confirmation for destructive
`--force`; `--yes` skips only that prompt. `--force` bypasses object age only,
never admission, root proof, quarantine, registry completeness, or
active-active proof.

Update both web GC documents and recovery/architecture text in the same commit.
Remove claims that force retains a one-hour minimum, because current force
bypasses age entirely. Document that history remains a root until explicit
prune.

**Verify**:
CLI parser/config tests show default 24h, explicit override wins, both scopes
prompt, and force cannot bypass a failed admission/root proof. Then run the GC
regression and format commands.

## Test plan

- Pure protocol property tests for arbitrary writer/sweeper/crash sequences.
- In-memory object-store CAS and heartbeat fault tests.
- Direct, protected, active-active, repack, import, restripe, replication,
  recovery, and workflow writer admission characterization.
- Multi-thread race tests with barriers at mark, admission, root re-read,
  delete, and release.
- Force and expired-writer quarantine tests.
- CLI/config/doc contract tests for one grace policy and both confirmations.

## Done criteria

- [ ] Every production writer to a GC-managed namespace holds admission before
  upload/adoption through publication.
- [ ] GC exclusivity is per bounded delete batch, not per full run.
- [ ] Root drift, admission loss, and cancellation fail closed before deletion.
- [ ] Force cannot bypass admission, root validation, or quarantine.
- [ ] Repo and bucket scopes share the configured 24-hour default and force
  confirmation behavior.
- [ ] Targeted tests, the full GC regression set, and format pass.
- [ ] `git diff --check` exits 0 and no unrelated files changed.

## STOP conditions

- A production writer to a collected namespace cannot be placed behind the
  shared protocol without changing its public authority model.
- The global admission CAS misses the retained writer-QPS target; partition the
  contract deliberately before shipping rather than accepting a hotspot.
- Provider semantics cannot supply holder-checked CAS and backend modification
  time for the admission record.
- A collector would need to hold exclusive admission while doing unbounded
  LIST, reachability traversal, or network downloads.
- An active-active service would bypass rather than validate the same epoch.
- `/Volumes/Workspace` or the dedicated target directory is unavailable.

## Maintenance notes

Admission is a correctness boundary, not an optimization. New remote mutation
surfaces must update the writer-map test. Reviewers should scrutinize lease-loss
cancellation, lock ordering, provider timestamps, and any path that uploads
before acquiring the guard.
