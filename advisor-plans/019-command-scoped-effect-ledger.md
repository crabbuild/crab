# Plan 019: Make effect allocation command-owned and constant-time

> **Executor instructions**: Execute in order and run every gate. This is an
> intentional cleanup of an unpublished API, not a compatibility exercise.
> Search all workspace consumers before changing public exports. Stop rather
> than adding an alias or second effect path. Update `advisor-plans/README.md`
> when done unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 892720ce6a6..HEAD -- crates/crab-cell-runtime/src/effects.rs crates/crab-cell-runtime/src/registry.rs crates/crab-cell-runtime/src/workflow.rs crates/crab-cell-runtime/src/workflow/activity.rs crates/crab-cell-runtime/src/queue crates/crab-cell-runtime/tests/effects.rs crates/crab-cell-runtime/tests/client.rs`

## Status

- **Priority**: P0
- **Effort**: M
- **Risk**: MED — effect identity and retry deduplication are durable contracts
- **Depends on**: none
- **Category**: correctness / performance / API
- **Planned at**: commit `892720ce6a6`, 2026-09-19
- **Implementation status**: implemented and verified; effect allocation is lazy until a command emits work

## Why this matters

Effect IDs derive from Cell incarnation, command sequence, and ordinal. A
command must therefore have exactly one ordinal allocator. Today
`CommandContext::effect_batch()` constructs a fresh allocator on every call,
so two batches reuse ordinal zero and fail only when the second insertion sees
different bytes. The API documents one batch but does not enforce it.

Every effect insertion also scans retained `sys_effects` rows by the unindexed
`created_sequence` column to recompute per-command count and bytes. At 100,000
retained effects, SQLite reports `SCAN sys_effects`; the audit measured roughly
16–18 ms per insertion check on the audit host. A maximum-size command repeats
that scan up to 128 times.

The correct owner is the command context: one allocator, one ordinal counter,
and one byte counter for the complete transaction.

## Current state

- `crates/crab-cell-runtime/src/registry.rs:160-204` stores command metadata but
  creates a new `EffectBatch` for every `effect_batch()` call.
- `crates/crab-cell-runtime/src/effects.rs:85-155` stores only `next_ordinal`.
- `crates/crab-cell-runtime/src/effects.rs:342-352` scans `sys_effects` for every
  insertion to enforce command limits.
- `crates/crab-cell-runtime/src/migrations/runtime.sql:35-52` has no
  `created_sequence` index.
- Queue dead-letter commands call `context.effect_batch()` in
  `crates/crab-cell-runtime/src/queue/api.rs:201` and `:247`.
- Workflow and maintenance use internal `EffectBatch` instances to share one
  allocator across a bounded transition loop; preserve that invariant.
- No Rust source outside `crab-cell-runtime` currently constructs `EffectBatch`.
  The crate is `publish = false`, so no shipped public compatibility has been
  established by the repository. Recheck tags before removal.

## Target contract

1. A registered command has exactly one effect ledger initialized when its
   `CommandContext` is constructed.
2. Public application commands emit through a narrow context method; they
   cannot construct or reset ordinals.
3. The ledger tracks `next_ordinal` and total encoded operation bytes in memory.
4. Limit validation occurs before insertion; transaction rollback remains the
   failure boundary.
5. Existing effect IDs, destination inbox deduplication, claim/lease/ack
   semantics, and stored schema remain byte-for-byte unchanged.

## Commands you will need

| Purpose | Command | Expected on success |
| --- | --- | --- |
| Effect tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-effects cargo test -p crab-cell-runtime --test effects --locked` | exit 0 |
| Client/command tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-effects cargo test -p crab-cell-runtime --test client --locked` | exit 0 |
| Workflow/Queue tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-effects cargo test -p crab-cell-runtime --test workflow --test workflow_api --test queue --locked` | exit 0 |
| Query-plan guard | `rg -n "created_sequence = \\?1" crates/crab-cell-runtime/src/effects.rs` | no aggregate scan match |
| Full runtime | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-effects cargo test -p crab-cell-runtime --release --locked` | exit 0 |
| Lint/format | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-effects cargo clippy -p crab-cell-runtime --all-targets --all-features --locked -- -D warnings && cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- `crates/crab-cell-runtime/src/effects.rs`
- `crates/crab-cell-runtime/src/registry.rs`
- `crates/crab-cell-runtime/src/queue/api.rs`
- Internal workflow/activity/scheduler callers only where required to consume
  the canonical allocator
- Effect, command, Queue, Workflow, and registry tests
- `crates/crab-cell-runtime/src/lib.rs` public export cleanup

**Out of scope**:

- Changing effect IDs or persisted `sys_effects`/`sys_inbox` formats
- Adding a `created_sequence` index as the primary fix
- Exactly-once external delivery claims
- Cross-Cell transactions
- A compatibility alias for `CommandContext::effect_batch()`
- Application-framework or node-host work from plans 022–023

## Git workflow

- Branch: `codex/019-command-effect-ledger`
- Commit style: `refactor(cell-runtime): own effects in command context`
- No push or PR unless instructed.

## Steps

### Step 1: Add regressions for allocator uniqueness and bounded work

Add registered-command tests that:

- emit two different effects through two separate helper calls in one command
  and observe ordinals zero and one, not an ordinal-reuse error;
- emit exactly `MAX_EFFECTS_PER_COMMAND`, then reject one more;
- reach exactly `MAX_EFFECT_BYTES`, then reject one more byte;
- retry the same request and return the recorded result without inserting
  duplicate effects;
- reject the command atomically when a limit is exceeded.

Add a query-plan regression or an authorizer/progress-handler test proving that
effect insertion work does not grow with unrelated retained effect rows. Seed
at least 100,000 unrelated rows in a transaction but do not assert a fragile
wall-clock threshold in the unit suite.

**Verify**: at least the two-helper test and retained-row work test fail on the
current implementation.

### Step 2: Give `CommandContext` one effect ledger

Initialize the allocator once in `Registry::execute_command_with_issue_time`
when constructing `CommandContext`. Expose the narrowest command-author API
that permits inserting a typed `EffectCommandIntent` without exposing raw
runtime tables or allowing the ordinal to reset.

Prefer an API shaped like `CommandContext::emit_effect(&mut self, &intent)`.
If internal primitives need simultaneous transaction and allocator access, add
a crate-private method returning disjoint fields; do not make the raw primitive
transaction public.

Remove `CommandContext::effect_batch()`. Make direct `EffectBatch` construction
crate-private unless a repository-wide consumer search proves a second current
owner. Rewrite integration tests to exercise registered commands rather than
preserving an otherwise-unused constructor solely for tests.

**Verify**: client, registry, Queue, Workflow, and effect tests compile and pass.

### Step 3: Enforce limits from ledger state

Add a byte counter to `EffectBatch`. Before consuming an ordinal or inserting
the row, checked-add the new operation length and enforce both limits. Remove
the aggregate `SELECT count(*), sum(length(operation))` from `effect_insert`.

Keep the existing effect-ID replay check: if an identical row is already
present, return its ID; if the same ID maps to different bytes, fail closed.
Do not weaken destination, expiry, sequence, or source validation.

**Verify**: the `rg` query-plan guard returns no aggregate scan and the seeded
retained-row regression passes under SQLite's progress handler/query plan.

### Step 4: Reconcile internal primitive composition

Update Queue dead-letter, Workflow, Activity, Cron, and maintenance callers so
each command/maintenance Tick still shares exactly one allocator. Do not expose
the private workflow `*_with_effects` functions merely to make the refactor
compile; public cross-primitive composition is decided in plan 022.

Search all calls with:

```bash
rg -n "EffectBatch::new|effect_batch\(" crates crab
```

Every production match must have a named command/Tick owner and exactly one
allocation site.

**Verify**: focused primitive suites and the full runtime release suite pass.

### Step 5: Run quality gates

Run Clippy, format, `git diff --check`, and inspect `git diff --numstat`. The
refactor should remove the SQL scan and should not add a second API path.

## Done criteria

- [x] A command cannot obtain two ordinal-zero allocators.
- [x] Effect insertion performs no retained-table count/byte scan.
- [x] Count and byte limits are checked before insertion with overflow handling.
- [x] Existing effect ID, retry, inbox deduplication, lease, and acknowledgement tests pass unchanged.
- [x] Public raw transaction access was not introduced.
- [x] Full runtime release tests, Clippy, format, and diff checks pass.

## STOP conditions

- A tagged release or external workspace consumer proves `EffectBatch::new` is a shipped contract.
- Removing the SQL aggregate cannot preserve limits for an internal path because that path lacks one allocator owner.
- The proposed context API requires exposing `Transaction` publicly.
- Any change would alter effect ID bytes or persisted schemas.

## Maintenance notes

Reviewers should trace every new effect-producing primitive to the command-owned
ledger. A new allocator construction site is a correctness concern, not a
convenience helper. Performance tests should use SQLite progress/query-plan
evidence for algorithmic work and reserve wall-clock thresholds for plan 024.
