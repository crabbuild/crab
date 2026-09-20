# Plan 018: Stop ready Queue messages from driving no-op maintenance commits

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving on. Stop
> on any listed STOP condition; do not add a Queue consumer or a compatibility
> path as an improvisation. Update this plan's status in `advisor-plans/README.md`
> when complete unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 892720ce6a6..HEAD -- crates/crab-cell-runtime/src/scheduler.rs crates/crab-cell-runtime/src/queue.rs crates/crab-cell-runtime/tests/scheduler.rs crates/crab-http-server/src/cells/scheduler.rs crates/crab-http-server/src/cells/scheduler/tests.rs`
> If the Queue deadline or server scheduler paths changed, compare the current
> code with the facts below and stop if ownership moved.

## Status

- **Priority**: P0
- **Effort**: S
- **Risk**: MED — an omitted maintenance deadline could strand expiry or lease work
- **Depends on**: none
- **Category**: bug / performance
- **Planned at**: commit `892720ce6a6`, 2026-09-19
- **Implementation status**: implemented and verified in the runtime/server test gates

## Why this matters

A ready Queue row currently makes its Cell immediately due to the fleet
scheduler, but a maintenance Tick does not claim Queue messages. The Tick
therefore records and publishes a new command without changing Queue state,
then leaves the same Cell immediately due. At fleet scale this converts idle
Queue backlog into continuous SQLite, LTX, object-store, and catalog work.

Consumer readiness and runtime maintenance are different responsibilities.
Queue `due_at_ms` belongs to explicit claimers; the Cell scheduler should wake
only for work it can perform: lease reclamation, terminalization, retention,
and dedup cleanup.

## Current state

- `crates/crab-cell-runtime/src/scheduler.rs:562-568` includes
  `min(due_at_ms)` for every ready Queue row.
- `crates/crab-cell-runtime/src/scheduler.rs:290-317` performs cleanup,
  ready-row terminalization, and expired-lease reclamation, but never claims a
  normal ready row.
- `crates/crab-http-server/src/cells/scheduler.rs:493-570` commits maintenance,
  then runs only Activity and Effect supervisors when `processed == 0`.
- `crates/crab-cell-runtime/src/executor.rs:416-466` records every successful
  command and advances `sys_meta.commit_sequence`, including an applied Tick
  whose processed count is zero.
- `crates/crab-cell-runtime/tests/scheduler.rs:443-490` is the existing deadline
  summary test pattern. `crates/crab-http-server/src/cells/scheduler/tests.rs`
  is the production scanner regression location.

The intended Queue maintenance deadlines are:

1. `lease_until_ms` for leased messages;
2. `expires_at_ms` for message terminalization or retention;
3. immediate logical time for ready messages already at the maximum attempt;
4. `retain_until_ms` for producer dedup rows.

Ordinary ready-message `due_at_ms` is deliberately absent unless a real Queue
consumer runner is later added as a separate, resource-admitted owner.

## Commands you will need

| Purpose | Command | Expected on success |
| --- | --- | --- |
| Runtime regression | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-queue cargo test -p crab-cell-runtime --test scheduler --locked` | exit 0; new deadline tests pass |
| Queue integration | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-queue cargo test -p crab-cell-runtime --test queue --locked` | exit 0 |
| Server scheduler | `npm ci --prefix packages/repository && npm run build --prefix packages/repository && CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-queue cargo test -p crab-http-server --locked --lib cells::scheduler::tests` | exit 0 |
| Lint | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-queue cargo clippy -p crab-cell-runtime -p crab-http-server --all-targets --locked -- -D warnings` | exit 0 |
| Format | `cargo fmt --all -- --check` | exit 0 |

Before any Cargo command, verify `$HOME/Workspace` is mounted and the selected
target directory is writable. Do not fall back to a repository-local target.

## Scope

**In scope**:

- `crates/crab-cell-runtime/src/scheduler.rs`
- `crates/crab-cell-runtime/tests/scheduler.rs`
- `crates/crab-http-server/src/cells/scheduler/tests.rs` only if necessary to
  prove the fleet-visible regression

**Out of scope**:

- Adding a background Queue consumer to `crab-http-server`
- Changing Queue delivery, ordering, lease, retry, or dead-letter semantics
- Changing the one-second fleet scan cadence
- Adding configuration or an environment toggle
- Preserving the erroneous ready-row deadline as a legacy mode

## Git workflow

- Branch: `codex/018-queue-scheduler-deadline`
- Commit style: `fix(cell-runtime): separate queue readiness from maintenance`
- Do not push or open a PR unless instructed.

## Steps

### Step 1: Add the failing deadline characterization

In `crates/crab-cell-runtime/tests/scheduler.rs`, add separate tests proving:

- one non-expired, below-attempt-limit ready row does not make maintenance due;
- an expired ready row is due and becomes dead during Tick;
- a ready row at `MAX_ATTEMPTS` is immediately due and becomes dead;
- a leased row wakes at `lease_until_ms`;
- acked/dead retention and Queue dedup retention still wake maintenance.

Use public Queue operations where possible. Direct SQL fixtures may be used for
states that cannot be constructed without advancing logical time, matching the
existing scheduler tests.

**Verify**: run the runtime regression command before the source fix. The first
test must fail because the current summary returns the ready row's `due_at_ms`.

### Step 2: Correct Queue deadline ownership

Remove normal ready-message `due_at_ms` from `scheduler_next_due_ms`. Add only
the minimum extra query needed to wake immediately for ready rows whose attempt
count is already terminal. Reuse existing Queue constants and indexes; do not
duplicate the maximum-attempt literal.

Check the query plan in a test or diagnostic. If the terminal-attempt predicate
requires scanning every ready row, add the smallest index that serves both the
terminalization query and deadline summary; do not replace an existing index
without checking `queue_claim` and retry ordering.

**Verify**: runtime scheduler and Queue integration commands both exit 0.

### Step 3: Prove the fleet does not republish idle Queue readiness

Add a server scheduler regression only if the runtime test does not exercise
the committed-command boundary. Construct a Queue Cell with one ordinary ready
message, run a bounded scanner cycle, and assert that no second maintenance
commit/root is published solely for that message. Then advance it to an actual
maintenance deadline and assert one bounded Tick occurs.

Do not add sleeps. Use the existing deterministic clock/storage fixtures in
`crates/crab-http-server/src/cells/scheduler/tests.rs`.

**Verify**: the focused server scheduler command exits 0.

### Step 4: Run affected broad proof

Run format, Clippy, the complete runtime suite, and full server library suite.

**Verify**:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-queue \
  cargo test -p crab-cell-runtime --release --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-queue \
  cargo test -p crab-http-server --locked --lib
```

Both commands exit 0; provider tests may remain explicitly ignored.

## Done criteria

- [x] An ordinary ready Queue row does not make maintenance due.
- [x] Expiry, attempts-exhausted, lease, retention, and dedup deadlines remain discoverable.
- [x] No scheduler scan publishes a new root solely because claimable Queue work exists.
- [x] Queue claimers still see and lease the same ready rows in the same order.
- [x] Runtime, Queue, server scheduler, full server library, format, and Clippy gates pass.
- [x] Only in-scope files and `advisor-plans/README.md` changed.

## STOP conditions

- A production Queue consumer exists outside the reviewed server scheduler path.
- Removing `due_at_ms` would leave a maintenance-owned state transition without another deadline.
- The fix requires changing Queue delivery semantics or adding a scheduler mode.
- An in-scope file drifted enough that the cited ownership no longer holds.

## Maintenance notes

Any future automatic Queue consumer must own its own admitted runner and may use
ready `due_at_ms` for consumer scheduling. It must not overload the maintenance
deadline summary unless the maintenance command itself can make progress.
