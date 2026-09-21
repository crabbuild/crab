# Plan 021: Make Workflow capacity and Queue status constant-time

> **Executor instructions**: Treat primitive SQL as persistent state. First
> determine whether any tagged release contains these schemas; follow the
> repository's hard-cutover policy only when no shipped contract exists. Do not
> add fallback readers or silently repair counters during normal requests.
> Update the plan index when complete.
>
> **Drift check (run first)**:
> `git diff --stat 892720ce6a6..HEAD -- crates/crab-cell-runtime/src/workflow.rs crates/crab-cell-runtime/src/queue.rs crates/crab-cell-runtime/src/migrations/workflow.sql crates/crab-cell-runtime/src/migrations/queue.sql crates/crab-cell-runtime/tests/workflow.rs crates/crab-cell-runtime/tests/queue.rs crates/crab-cell-runtime/tests/migration.rs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH — incorrect counters could reject valid work or report false state
- **Depends on**: plan 018
- **Category**: performance / persistence
- **Planned at**: commit `892720ce6a6`, 2026-09-19
- **Implementation status**: implemented and verified with counter-drift diagnostics and migration tests; the tag audit found no shipped Cell-runtime schema, so the version-one trigger/counter schema is an intentional hard cut rather than a compatibility migration

## Why this matters

Every Workflow transition currently counts the complete `workflow_events`
table to enforce the 100,000-event Cell limit. Queue operator status scans all
messages and computes four filtered counts. Both operations become slower as
durable history grows; Workflow places that cost directly on the serialized
mutation path.

SQLite can maintain exact counters transactionally with the rows they
summarize. This plan makes command-path capacity checks and Queue status O(1)
while retaining a separate explicit verifier for corruption and migration.

## Current state

- `crates/crab-cell-runtime/src/workflow.rs:1132-1139` executes
  `SELECT count(*) FROM workflow_events` before each new event.
- `crates/crab-cell-runtime/src/workflow.rs:1154` is the sole event insertion;
  `crates/crab-cell-runtime/src/workflow.rs:509` deletes a run's events during restart.
- `crates/crab-cell-runtime/src/queue.rs:451-477` scans all Queue messages for
  ready/leased/acked/dead counts.
- Queue state changes occur at send, claim, lease action, reclaim,
  terminalization, purge, cleanup, and redrive sites in
  `crates/crab-cell-runtime/src/queue.rs`.
- `queue_control` is already a singleton metadata row in
  `crates/crab-cell-runtime/src/migrations/queue.sql`.
- Primitive schemas are installed through application migration descriptors in
  typed modules. Current `crab-http-server` does not install Queue or Workflow
  schemas, but tags and all consumers must still be audited before a hard cut.

## Target contract

- A Workflow singleton stores exact total event rows for that Cell.
- Queue singleton metadata stores exact counts for all four message states.
- SQLite triggers maintain counters in the same transaction as row changes, so
  every mutation path—including future ones—participates automatically.
- Counter values are nonnegative and their sum equals the Queue row count.
- A migration/backfill derives initial counters once and validates them before
  the new schema becomes active.
- A diagnostic verifier recomputes counts explicitly; ordinary command/query
  paths never repair or rescan.

## Commands you will need

| Purpose | Command | Expected on success |
| --- | --- | --- |
| Tag audit | `git tag --contains 892720ce6a6 && git log --all -S 'CREATE TABLE workflow_events' --oneline --decorate` | reviewed result recorded in commit/PR notes |
| Workflow tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-counters cargo test -p crab-cell-runtime --test workflow --test workflow_api --locked` | exit 0 |
| Queue tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-counters cargo test -p crab-cell-runtime --test queue --locked` | exit 0 |
| Migration tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-counters cargo test -p crab-cell-runtime --test migration --locked` | exit 0 |
| Plan guard | `rg -n "SELECT count\(\*\) FROM workflow_events|count\(\*\) FILTER" crates/crab-cell-runtime/src` | no ordinary hot-path match |
| Full runtime | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-counters cargo test -p crab-cell-runtime --release --locked` | exit 0 |
| Quality | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-counters cargo clippy -p crab-cell-runtime --all-targets --all-features --locked -- -D warnings && cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- Workflow and Queue migration SQL
- Workflow capacity and Queue status implementations
- Primitive migration, Workflow, Queue, scheduler, and recovery tests
- An explicit internal consistency verifier used by migrations/doctor/qualification

**Out of scope**:

- Changing the 100,000 Workflow event limit
- New Queue states, FIFO guarantees, or delivery behavior
- Periodic counter reconciliation in production
- Fallback to the old full scans
- Changing unrelated runtime schema or effect accounting
- Adding operator configuration

## Git workflow

- Branch: `codex/021-primitive-counters`
- Commit style: `perf(cell-runtime): maintain primitive cardinality counters`
- Keep migration/schema and source/test changes in reviewable commits.

## Steps

### Step 1: Establish the migration decision

Search tags, release manifests, application migration descriptors, tests, and
all workspace consumers for Queue/Workflow schema version 1. Record one of:

- **unshipped**: update the existing version-one primitive schema and delete
  obsolete fixtures; or
- **shipped**: add a new ordered application migration that creates/backfills
  counters and triggers atomically.

Do not infer shipment from `main`. A release tag or explicit stored-data
contract is required. If shipped consumers cannot all receive the migration,
stop.

**Verify**: migration decision and affected descriptors are listed in the PR
description; no compatibility branch exists in runtime code.

### Step 2: Add counter integrity regressions

Before implementing, add table-driven tests covering every Queue transition:
send, claim, retry, extend, ack, terminal failure, expired lease reclaim,
expired ready terminalization, purge, redrive, and cleanup. After each action,
compare stored counters against one explicit test-only aggregate.

For Workflow, cover start, signal, timer, activity completion/failure, duplicate
events, rejection/rollback, restart deletion, and exact capacity exhaustion.
Rollback tests must prove failed transactions leave counters unchanged.

**Verify**: tests fail while counter columns/tables do not exist.

### Step 3: Add transactionally maintained counters

Use SQLite triggers attached to the canonical row tables:

- Workflow `AFTER INSERT` and `AFTER DELETE` event triggers adjust one
  nonnegative singleton counter.
- Queue insert/delete/state-update triggers adjust the four state counts.

Use `STRICT` tables/checks consistent with neighboring schemas. Abort the
transaction on underflow or an unknown state. Do not manually update counters
at every Rust call site; that is vulnerable to future path drift.

For a shipped migration, create counters, backfill from one aggregate scan,
validate, create triggers, and activate the schema in one migration transaction.

**Verify**: transition and rollback tests pass.

### Step 4: Switch hot paths to singleton reads

Replace Workflow's full count in `next_sequence` with the maintained value.
Replace Queue's filtered aggregate with the `queue_control`/counter row.
Retain checked integer conversion and corruption errors.

Add `EXPLAIN QUERY PLAN` regressions or SQLite progress-handler tests proving
both reads remain constant as 100,000 unrelated rows are added. The source
guard must find no old aggregate query in ordinary runtime paths; an explicit
diagnostic verifier may contain one and must be clearly named.

**Verify**: source guard and 100,000-row work tests pass.

### Step 5: Add explicit verification and recovery proof

Implement a bounded-entry diagnostic/qualification function that compares
counters with recomputed aggregates and reports mismatch without mutating the
database. Use it after migration and in plan 024 qualification. Do not run it
on every request.

Publish, restore, and reopen Queue/Workflow Cells; verify counters and visible
states survive exact-root recovery. Inject a transaction failure after a row
change and before commit; verify neither row nor counter advances.

**Verify**: migration, publication, owner-loss, and source-loss tests pass.

### Step 6: Run broad proof

Run the full release runtime suite, Clippy, format, and `git diff --check`.
Review every SQL trigger and migration as a persistent contract.

## Done criteria

- [x] Workflow transition capacity checks do not scan `workflow_events`.
- [x] Queue status does not scan `queue_messages`.
- [x] All Queue transitions and Workflow event lifecycle operations preserve exact counters.
- [x] Transaction rollback, retry, duplicate requests, and recovery preserve counter equality.
- [x] Migration/backfill behavior matches the recorded shipment decision.
- [x] Explicit verification reports mismatch without silently repairing it.
- [x] Runtime release, migration, Clippy, format, and diff gates pass.

## STOP conditions

- Shipment audit is inconclusive.
- A supported deployment cannot receive the schema migration atomically.
- SQLite triggers conflict with the runtime authorizer or LTX capture contract.
- A counter requires eventual rather than transactional consistency.
- The Queue scheduler semantics from plan 018 are not yet stable.

## Maintenance notes

Every future direct mutation of `queue_messages` or `workflow_events` must be
covered by trigger-preservation tests. The explicit aggregate verifier is for
migration, doctor, and qualification; using it as a request-path fallback would
reintroduce the scaling defect.
