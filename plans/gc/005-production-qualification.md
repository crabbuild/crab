# Plan 005: Qualify current GC for production scale and failure

> **Executor instructions**: This is the release gate for standalone GC Plans 001–004, not a
> substitute for their tests. Run only in isolated approved environments. Never
> run `crab gc --scope=bucket` against a shared/customer bucket. Stop on every
> STOP condition and update `plans/gc/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat ff7792a4..HEAD -- crab/src/cmd/gc crab/src/git/push.rs crates/crab-auth-server crates/crab-coordination crates/crab-metadata crab/scripts/e2e .github/workflows crab/docs packages/web/content/docs/cli`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/gc/001-writer-admission.md`,
  `plans/gc/002-bounded-resumable-engine.md`,
  `plans/gc/003-shard-closures.md`,
  `plans/gc/004-strict-inventory.md`
- **Category**: tests, perf, docs
- **Planned at**: commit `ff7792a4`, 2026-08-22

## Why this matters

The current 84-test GC set gives useful in-memory behavior proof but does not
qualify long concurrent writers, process death during deletion, bounded memory
at high cardinality, or real S3/GCS/Azure inventory and error semantics. This
plan turns those adoption risks into retained release evidence and blocks
provider/scale claims when any row is missing.

## Current state

- At `ff7792a4`,
  `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib cmd::gc --locked`
  passes 84 tests. Coverage includes grace/force, current/history/journal/
  workflow reachability, registry completeness, active-active proof, adaptive
  listing, cancellation during enumeration, maintenance leases, and
  coordinator-protected keys.
- Those tests use in-memory stores. There is no retained real-provider race of
  a long push against repo/bucket GC, no high-cardinality RSS/scratch proof, and
  no bucket partial-delete resume evidence.
- `.github/workflows/replica-live-evidence.yml` and existing RustFS/concurrent
  push scripts show the repository's evidence-artifact pattern. Extend that
  pattern; do not print credentials or hide results in a developer terminal.
- This roadmap targets the current Unified layout and can ship independently.
  No future layout, recipe tree, partitioned metadata, or migration plan is a
  prerequisite or release consumer.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Targeted GC | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib cmd::gc --locked` | all pass |
| Full Rust | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make test` | all pass |
| Clippy | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make clippy` | exit 0 |
| Web links | `cd packages/web && npm run check:links` | exit 0 |
| Qualification unit tests | `python3 -m unittest discover -s crab/scripts/e2e -p 'test_gc_qualification.py'` | all pass |

Broad/provider commands require dedicated credentials, isolated prefixes, an
approved object-count/cost budget, and cleanup authority. Record the exact live
commands in the runbook/CI, not in this generic plan.

## Scope

**In scope**:

- `crab/scripts/e2e/gc_qualification.py` and
  `crab/scripts/e2e/test_gc_qualification.py` (create)
- Rust fault-injection/integration tests near GC/admission/journal owners
- `.github/workflows/gc-provider-qualification.yml` (create)
- structured GC metrics/evidence output in `crab/src/cmd/gc/`
- provider GC runbooks under `crab/docs/`
- CLI/storage docs under `packages/web/content/docs/cli/`
- this standalone roadmap index, provider runbooks, and current GC docs

**Out of scope**:

- Production/customer deletion.
- Provider support without real-provider evidence.
- Editing baselines, inventories, snapshots, or expected-failure files to make
  a gate green.
- Extrapolating materialized throughput from a metadata-only simulation without
  labelling it.
- Automatically provisioning or deleting cloud accounts/buckets.

## Git workflow

- Branch: `gc/005-production-qualification`
- Commits: harness/faults, metrics, provider CI, runbooks/docs.
- Evidence workflows upload artifacts but never credentials.

## Steps

### Step 1: Define the release evidence schema and hard gates

Emit one machine-readable evidence bundle containing Crab commit/version,
current storage contract (`Unified`), provider/region, emulator-versus-real,
`object_store` version, inventory
identity, root/admission epochs, object/row/byte counts, configured RAM/scratch/
concurrency budgets, peak RSS/scratch/open files, LIST/HEAD/GET/DELETE counts,
retries, writer wait time, batch duration, journal/resume counts, candidate/
deleted/retained/failure totals, and final fsck result.

Set these pass/fail gates:

- zero referenced, historical, grace, quarantined, or admitted-writer objects
  deleted in every run;
- zero deletion when enumeration/report/root/admission validation is incomplete;
- peak engine-managed RAM and scratch remain within configured hard budgets;
- cancellation/process death resumes to exactly one durable outcome per
  candidate;
- complete Unified closure runs perform zero referenced-shard body GETs;
- inventory-backed current-layout runs perform zero recursive namespace LISTs;
- a writer pause is bounded by one configured delete batch, never full-run
  duration;
- post-run `fsck` and byte-identical clone/hydrate succeed.

Do not set an unmeasured throughput promise. Establish current GC baselines in
the evidence harness before setting release latency/throughput thresholds.

**Verify**: schema tests reject a missing gate, unknown provider/layout, secret-
like value field, or emulator evidence labelled real.

### Step 2: Build deterministic fault and race scenarios

Build an isolated fixture with current/history-only roots, workflow artifacts,
referenced and orphaned packs/shards/xorbs, recent objects, expired-writer
quarantine, and file-index rows. Add deterministic barriers and injected faults
at writer admission, upload, registry union, manifest CAS, inventory read,
journal seal, sweep acquisition, root re-read, each delete result, outcome
checkpoint, and release.

Scenarios include direct and protected pushes lasting longer than the normal
grace, force planning, active-active coordinator publication, repack, history
prune/restore, process kill after partial delete, cancellation at every phase,
transient retry, terminal error, and concurrent resume attempts. Assert final
remote keys and roots, not only exit codes.

**Verify**:
`python3 -m unittest discover -s crab/scripts/e2e -p 'test_gc_qualification.py'`
→ deterministic orchestration tests pass without network credentials.

### Step 3: Prove bounded scale separately from materialized provider scale

Run a deterministic synthetic inventory/reachability workload whose rows exceed
available RAM by at least 10x and whose cardinality reaches the explicitly
declared current-product target. Retain RSS, scratch, open-file, phase-time, and
digest artifacts. Run a
materialized isolated object-store workload at the largest object count approved
by the cost/environment owner; label its exact count and do not extrapolate it
to a larger claim.

Compare serial and configured concurrency for LIST/Parquet read, external join,
and delete batches. Tune only from evidence. A concurrency increase that raises
throttling/retry cost without improving wall time is rejected.

**Verify**: the scale run exits non-zero on any budget exceedance and repeats to
the same candidate digest and terminal counts.

### Step 4: Qualify S3-compatible, GCS, and Azure independently

For each provider, use a dedicated account/project, bucket/container, and unique
test prefix. Prove admission CAS/timestamps, inventory manifest/completion/
Parquet parsing, object identity behavior, listing used by diagnostics, delete
idempotency/error classification, process resume, and cleanup limited to the
test prefix. Run one real service row per advertised provider; emulator evidence
is supplementary.

Inject changed inventory shard identity, stale report, throttling, dropped
connection, permission denial, object replacement, and cancellation. Verify the
collector fails closed or resumes as specified. Never weaken a gate for a
provider abstraction mismatch; mark the provider unsupported.

**Verify**: CI artifacts contain one complete matrix row per provider with no
credentials and a final pass/fail decision.

### Step 5: Add observability and operator recovery drills

Expose phase, progress, run ID, source/root/admission identities, memory/scratch
budgets, requests/retries, candidate/deleted/retained/failure counts, and resume
instructions in structured output. Logs identify a provider-qualified error
without leaking object contents or credentials. Add alerts/runbook thresholds
for stale inventory, admission contention, root churn, repeated journal resume,
terminal delete failure, scratch exhaustion, and closure coverage gaps.

Have an operator follow dry-run, review, execute, interrupt, resume, fsck, and
history restore using only published docs. Record ambiguities as doc/test fixes.

**Verify**: structured-output schema tests pass; web link check passes; the
runbook drill produces a complete evidence bundle.

### Step 6: Publish a standalone current-GC release gate

Make GC release readiness depend only on this roadmap's matching successful
provider evidence row. Release notes must state the current Unified layout,
supported providers, required closure backfill/inventory setup, journal
retention, and known scale envelope. Remove stale claims of automatic repack, 1h versus 24h
grace, force minimum grace, and old lock paths wherever the scoped search finds
them.

**Verify**: `rg` finds one consistent GC policy in CLI/docs; the standalone GC
index references the retained evidence artifacts; full targeted tests, clippy,
and link checks pass.

## Test plan

- Evidence-schema negative tests.
- Deterministic multiwriter/fault state matrix.
- 10x-RAM synthetic cardinality and repeatable digest.
- Approved materialized object-store scale run.
- Real S3-compatible, GCS, and Azure contract rows.
- Operator interrupt/resume/fsck/restore drill.
- Full GC suite plus broad Rust/clippy/docs gates in dedicated CI.

## Done criteria

- [ ] Every hard gate has a retained machine-readable artifact.
- [ ] Direct/protected/active-active writers survive long and force GC races.
- [ ] Crash, cancellation, transient error, and terminal partial failure resume
  with exact durable accounting.
- [ ] The declared current-product synthetic cardinality stays within configured
  RAM/scratch bounds.
- [ ] Every advertised provider has a passing real-provider row.
- [ ] Post-GC fsck and byte-identical readback pass.
- [ ] The standalone GC release gate and public docs require this evidence.
- [ ] Full tests, clippy, format, links, and `git diff --check` pass in CI.

## STOP conditions

- Credentials, isolation, cleanup scope, or cost approval is unclear.
- A command could address a shared/customer bucket or broad prefix.
- Any protected object is deleted, even if restore succeeds afterward.
- A budget/gate is proposed as advisory rather than process-failing.
- Emulator or synthetic evidence is presented as real-provider/materialized
  qualification.
- A provider row fails and the proposed response is to advertise it anyway.

## Maintenance notes

Retain qualification per release and rerun it when provider adapters,
`object_store`, Parquet, admission/journal schemas, or deletion batching change.
Evidence expires as dependencies and provider behavior change.
