# Plan 005: Qualify GC under real races, crashes, scale, and providers

> **Executor instructions**: This is the release gate for Plans 001–003; include
> Plan 004's rows before claiming inventory-backed deletion. Do not call GC
> production-ready because unit tests or a RustFS happy path pass.
> Every advertised writer path and provider must have reproducible evidence.
> Keep all qualification repositories, target directories, buckets, and
> generated evidence outside the Crab checkout.

**Drift check (run first):**
`git diff --stat b738f3b2..HEAD -- crab/src/cmd/gc crab/src/cmd/repack.rs crab/src/cmd/recover.rs crab/src/cmd/history_recovery.rs crab/src/cmd/restripe.rs crab/src/restripe crab/src/git/push.rs crab/src/replication crates/crab-workflow crates/crab-coordination crates/crab-storage crates/crab-metadata crab/scripts/e2e .github/workflows packages/web/content/docs`

If the diff changes a writer, storage layout, run schema, or CLI contract, stop
and reconcile the evidence matrix before running destructive tests.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-close-writer-gc-fence.md`,
  `plans/002-durable-bounded-gc-engine.md`, and
  `plans/003-persist-shard-closures.md`; Plan 004 is required only for an
  inventory-backed release claim.
- **Category**: verification/release
- **Planned at**: commit `b738f3b2`, 2026-08-22

## Delivery status

The deterministic fixture/evidence harness and evidence validator are
implemented in `crab/scripts/e2e/gc_qualification.py` and
`crab/scripts/e2e/validate_gc_evidence.py`. Unit and focused integration suites
pass for the delivered fence/journal/closure paths; real-provider race,
crash-resume, and bounded high-cardinality evidence is still required before a
production qualification claim.

## Why this matters

The current repository has unit tests for grace, force, protected keys, and
maintenance contention, plus a production-scale RustFS script for push/clone.
It does not retain evidence that a GC run is safe when a writer overlaps a
delete, a process dies after partial deletion, a namespace exceeds RAM, or a
provider inventory is incomplete. This plan converts the hardening work into
repeatable release gates with bounded measurements and data-integrity proofs.

## Current state

- `crab/src/cmd/gc/mod.rs` and `crab/src/cmd/gc/bucket.rs` contain focused
  in-memory tests, but no crash/resume or durable-run qualification matrix.
- `crab/scripts/e2e/run_production_scale_rustfs.py` exercises RustFS push,
  clone, cache, and scale flows; it does not inject GC races, kill points, or
  provider-list/delete faults.
- `.github/workflows/` contains GC-related paths and jobs, but no retained
  report that records peak memory, temporary bytes, open files, requests,
  delete outcomes, closure GETs, or post-GC fsck/clone evidence.
- S3, GCS, and Azure credentials/configuration are environment-specific. A
  missing provider or permission is a qualification blocker, not permission
  to silently substitute an emulator for a production-provider claim.

## Evidence contract

Each run writes a versioned, machine-readable report outside the source tree.
The report must include:

- Crab commit, plan/run schema versions, provider and endpoint class, isolated
  bucket/container/prefix, fixture seed, and UTC start/end times;
- object counts and byte totals for manifests, shards, xorbs, file indexes,
  histories, workflow artifacts, and orphan candidates;
- peak RSS, temporary bytes, open-file high-water mark, queue high-water mark,
  list/head/get/delete request counts, retries, throttling, and wall time;
- writer/fence epochs, lease-loss/retry events, run phases, batch IDs, and one
  outcome for every candidate object;
- referenced-shard body GETs, closure records consumed, report rows accepted,
  and rejected rows (an executable strict inventory run must report zero); and
- post-run `fsck`, fresh clone, hydrate/readback hashes, and the exact commands
  used. Redact credentials, signed URLs, and object contents.

An evidence file is valid only when its run journal, provider fixture, and
post-run integrity output are present. A green process exit without those
artifacts is a failed qualification.

## Commands you will need

Run from the isolated worktree or a clean checkout. Use the required external
target directory on every Cargo invocation:

| Purpose | Command | Expected on success |
|---|---|---|
| Focused GC tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked gc` | current and hardened GC tests pass |
| Full Rust proof | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked` | no unrelated regression |
| Format/lints | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo fmt --all -- --check && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo clippy -p crab --locked --all-targets -- -D warnings` | both exit 0 |
| RustFS matrix | `python3 crab/scripts/e2e/gc_qualification.py --provider rustfs --work-dir /Volumes/Workspace/CrabBuild/gc-evidence` | complete local matrix and evidence |
| Provider matrix | `python3 crab/scripts/e2e/gc_qualification.py --providers s3,gcs,azure --work-dir /Volumes/Workspace/CrabBuild/gc-evidence` | one independently passing report per configured provider |
| Evidence validation | `python3 crab/scripts/e2e/validate_gc_evidence.py /Volumes/Workspace/CrabBuild/gc-evidence` | schema, redaction, and required proof checks pass |

The script names above are deliverables of this plan. If the repository's
existing harness has a different entry point, update the commands and docs in
the same change; never leave a command that cannot be run from a clean tree.

## Scope

**In scope**:

- a deterministic fixture generator and qualification runner using the real
  Crab CLI and object-store APIs;
- `crab/scripts/e2e/gc_qualification.py`,
  `crab/scripts/e2e/validate_gc_evidence.py`, and focused fixture helpers;
- fault injection at writer admission, root publication, fence acquisition,
  plan seal, mark/candidate batch, delete batch, journal outcome, and resume;
- resource/request instrumentation and evidence validation;
- `.github/workflows/` wiring beside the existing RustFS/replica evidence jobs,
  provider-safe setup, redaction, and operator runbooks;
- RustFS plus every provider whose destructive GC support is advertised.

**Out of scope**:

- changing GC correctness behavior solely to make a test pass;
- claiming a production provider from an emulator, unit mock, or an unavailable
  credential set;
- PB layouts, migration plans, cache-service deployment, or unrelated benchmark
  tuning;
- deleting user data outside an explicitly created fixture prefix.

## Steps

### Step 1: Build an isolated evidence harness

Create a fixture repository with current and historical refs, retained and
unreachable manifests, shared shards/xorbs, file indexes, ref journals,
workflow artifacts, cache entries, orphaned bulk objects, and objects inside
and outside the grace period. Include files that share chunks and multiple
generations of the same ref. Seed expected live roots and byte hashes before
the sweep.

Run every case in a newly created bucket/container or unique prefix. Capture
the durable GC run journal, provider request counters, process metrics, and
CLI JSON/JSONL. Verify the fixture itself with `fsck` before any destructive
case.

**Verify**: two runs with the same seed produce equivalent candidate identity
and post-run hashes; no fixture writes occur in the Crab checkout or a shared
qualification repository.

### Step 2: Exercise the complete writer race matrix

For each row, hold the writer beyond the grace period and attempt a guarded
GC delete at the same time. Cover direct object-store push, protected receive,
active-active coordinator write, repack, restripe, recovery restore, history
recovery, replica publication, workflow artifact/cache publication, and the
ref-registry repair/closure backfill path. Test both a writer that commits and
one that aborts after staging.

Assert that the fence/epoch protocol either waits, marks the candidate
protected, or fails closed. It must never delete a staged, newly published, or
coordinator-protected object. After each case run `fsck`, fresh clone, and
hydrate/readback comparison against the pre-run hashes.

**Verify**: every matrix row has a durable run outcome, no unexpected delete,
and a post-run integrity artifact. Repeat each timing-sensitive row at least
ten times with different scheduling seeds; one nondeterministic failure is a
release blocker.

### Step 3: Exercise crashes, cancellation, and provider faults

Terminate the GC process at every durable phase boundary: before/after root
snapshot, plan seal, each mark fragment, candidate batch seal, fence renewal,
delete request, per-object outcome, run finalization, and reconciliation.
Also inject cancellation, lease loss, timeout, throttling, partial LIST, stale
inventory, missing closure, conditional-identity mismatch, and transient
delete failures.

Resume only from the durable run ID. Assert that already-successful objects
are not deleted twice, failed/unknown objects remain safe, a lost fence pauses
the run, and a corrupt or incomplete journal is refused. Verify retry classes
and operator-visible JSON/JSONL explain whether to resume, repair, or abandon
the run.

**Verify**: every kill point reaches a terminal `completed`, `paused`, or
`failed-closed` state with one outcome per candidate and no local state needed
for recovery. Re-run the same resume command to prove idempotence.

### Step 4: Prove bounded memory and I/O at scale

Generate cardinalities at least 10× the available RAM and a second fixture
with a high fan-out shard/file-index closure. Measure RSS, temporary bytes,
open files, queue depth, provider requests, and wall time while varying batch
and concurrency limits. Run with closures present and with closure coverage
intentionally missing.

The pass criterion is an explicit budget chosen before the run: peak RAM and
temporary bytes remain bounded independent of object count, request
concurrency stays within the provider budget, and no full candidate/reachability
collection is rebuilt during resume. Closure-complete bucket GC must record zero
referenced-shard body GETs; a missing closure must fail closed rather than
silently downloading every shard.

**Verify**: evidence includes metric samples and fixture cardinality; a 2×
cardinality run does not cause an unbounded memory slope or open-file leak.

### Step 5: Qualify provider behavior without widening scope

Run RustFS for deterministic fault and scale coverage. For each advertised S3,
GCS, and Azure destructive source, run in a dedicated test bucket/container
and prefix with credentials supplied only through the existing environment
contract. Validate live-list and, where enabled, strict inventory modes
separately. Record provider version/endpoint class, conditional-delete or
version identity support, listing consistency observations, and all request
failures.

A provider with missing credentials, unsupported conditional identity, stale or
partial inventory, or an undocumented schema is `unsupported` for destructive
GC, not a pass and not an automatic fallback. Keep the evidence and runbook
explicit about which provider/format is release-qualified.

**Verify**: no provider claim is made without a passing report and integrity
proof for that provider. Emulators are labeled supplementary and cannot satisfy
the production-provider row.

### Step 6: Make the release gate executable

Add a manual/CI workflow that runs focused tests on every change to GC,
coordination, storage, metadata, writer, or provider-adapter code, and runs
RustFS fault/scale qualification on a scheduled or manually approved job.
Provider jobs must be opt-in, isolated, and never print secrets. Upload only
redacted evidence with the commit and run schema identifiers.

Update GC docs, operator runbooks, and `crab gc --help` with the qualification
levels, supported provider rows, evidence location, resume/repair commands,
resource budgets, and the rule that a missing gate blocks destructive GC.

**Verify**: a clean checkout can run the documented local matrix; CI fails when
required evidence is missing, malformed, unredacted, or reports a delete from
an unsealed/stale source.

## Test plan

- Deterministic unit/property tests for fixture roots, evidence schema,
  redaction, cardinality accounting, and run-outcome uniqueness.
- Object-store integration tests for every writer race, crash point, lease loss,
  partial listing, conditional identity mismatch, and resume path.
- Post-GC `fsck`, fresh clone, hydrate/readback, and byte-hash comparisons for
  every destructive scenario.
- Resource-budget assertions for RSS, temporary bytes, open files, queue depth,
  request concurrency, and referenced-shard body GETs.
- RustFS fault/scale matrix plus independently reported S3, GCS, and Azure
  rows for each advertised destructive mode.
- Focused tests, full `cargo test`, format, clippy, evidence validation, and
  documentation/link checks as applicable.

## Done criteria

- [ ] Every current writer path has a passing race row and integrity proof.
- [ ] Every crash/fault boundary resumes or fails closed with durable evidence.
- [ ] Scale runs prove predefined RAM, temp-space, file, and provider-request
      budgets; closure-complete bucket GC records zero shard-body GETs.
- [ ] RustFS deterministic coverage passes and each advertised real provider
      has an independent passing evidence row; unsupported providers are
      explicitly blocked from destructive GC.
- [ ] CI/manual workflows validate evidence schema, redaction, and required
      post-run proofs and cannot report a green run with missing artifacts.
- [ ] Docs, CLI help, and recovery runbooks match the shipped controls and
      provider matrix.
- [ ] No source outside Scope changed and no unrelated baseline failure was
      hidden or waived.

## STOP conditions

- A writer path, kill point, provider, or destructive scope cannot be isolated
  safely or lacks a post-run integrity proof.
- A race test ever deletes an object later shown to be live, staged, protected,
  in grace, or covered by an unresolved lease.
- Resume requires local filesystem state, duplicates a successful delete, or
  accepts an incomplete/corrupt journal.
- Memory, temporary bytes, open files, queue depth, or provider requests exceed
  the predeclared budget or grow unbounded with cardinality.
- A provider schema/identity contract is uncertain, credentials are absent, or
  evidence would expose secrets; mark that provider unsupported and stop its
  release claim.
- The documented harness cannot run from a clean checkout, or any verification
  command fails twice after a reasonable fix attempt.

## Maintenance notes

Keep the writer matrix synchronized with every new remote writer and keep one
fixture seed stable for regression comparisons. Requalify after changes to the
GC run schema, coordination lease semantics, storage layout, object-store
dependency, provider inventory schema, or delete/list concurrency. Retain
redacted evidence long enough to compare release candidates, and remove only
qualification artifacts created under the dedicated evidence directory.
