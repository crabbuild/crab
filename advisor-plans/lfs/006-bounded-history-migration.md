# Phase 5: Make LFS history migration bounded, transactional, and local-first

> **Executor instructions**: History rewriting is destructive. Preserve original refs and working-tree state until one atomic commit boundary. Run every verification and update the Phase 5 row in `advisor-plans/lfs/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crab/src/lfs/migrate.rs crab/src/cmd/lfs/migrate.rs crab/src/cmd/lfs/mod.rs crab/docs/guides/lfs.md`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: Phases 0 and 1
- **Category**: migration, correctness, perf
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

Crab exposes Git LFS-compatible migration commands, but the rewrite pipeline currently retains the complete fast-export model and a second complete serialized stream in memory. It can upload objects while history is still being rewritten, restores refs one by one on failure, and ignores final checkout failure. Production parity requires repositories larger than memory to migrate with explicit local/remote side effects and transactional recovery.

## Current state

- `crab/src/lfs/migrate.rs:1173` runs `git fast-export` with `.output()` and returns the complete stdout `Vec<u8>`.
- `crab/src/lfs/migrate.rs:1398` parses the complete stream into blobs/commits; `crab/src/lfs/migrate.rs:1498` clones blob data during conversion.
- `crab/src/lfs/migrate.rs:1544` serializes the rewritten model into another complete `Vec<u8>` before `fast-import`.
- `crab/src/lfs/migrate.rs:1380` may resolve a remote store and `lfs_pointer_for_content` can upload while rewrite is incomplete.
- `crab/src/lfs/migrate.rs:1551` restores refs best-effort after import failure; `crab/src/lfs/migrate.rs:1567` discards the checkout result.
- Phase 0 must decide whether Git LFS parity means migration stages objects locally by default and publishes only on a later push.

## Commands you will need

| Purpose | Command | Expected |
|---------|---------|----------|
| Migration tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib lfs::migrate --locked --no-default-features` | all migration tests pass |
| Broad LFS tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib lfs --locked --no-default-features` | all pass |
| Check | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo check -p crab --locked --no-default-features` | exit 0 |

## Scope

**In scope**:
- `crab/src/lfs/migrate.rs`
- migration CLI policy in `crab/src/cmd/lfs/`
- focused migration integration tests
- migration behavior and recovery documentation

**Out of scope**:
- changing pointer syntax or object layout
- automatic remote ref updates
- retaining current automatic object upload unless Phase 0 explicitly approves it as a shipped contract
- a second legacy migration engine

## Git workflow

- Branch: `advisor/lfs-phase-5-migration`
- Commit planner/preflight, streaming pipeline, transaction/recovery, then docs/tests.
- Do not push unless instructed.

## Steps

### Step 1: Add a read-only migration plan and capacity preflight

Before mutation, report selected refs, expected old OIDs, matching blob count, unique logical bytes, estimated temporary disk, local LFS growth, remote transfer policy, worktree cleanliness, and rollback location. Refuse to start without enough free space or with unsupported ref/worktree state. The plan must be deterministic and machine-readable.

**Verify**: dry-run on a fixture reports exact counts and aborts a deliberately under-capacity profile before writing refs or LFS objects.

### Step 2: Stream or spool fast-export records

Replace complete stdout capture with a parser over child stdout. Retain structural metadata only; spool large blob bodies to unique temporary files and reference them by path/mark. Bound record length, declared data length, mark count, path length, and total temporary storage. Drain stderr concurrently to avoid deadlock.

**Verify**: a history larger than the configured memory budget migrates while instrumentation remains below the budget; malformed/truncated export fails before ref changes.

### Step 3: Stage generated LFS objects locally

Hash source bytes once while streaming into standard local LFS cache staging, verify exact size, then replace the export record with the canonical pointer. Default behavior should not upload during rewrite unless Phase 0 documented an explicit, separately authorized mode. Deduplicate repeated content by OID.

**Verify**: repeated blobs create one local object; no remote PUT occurs in local-first mode; every pointer OID/size matches staged bytes.

### Step 4: Stream rewritten records into fast-import

Write records incrementally to a child `git fast-import` stdin instead of building `output_buf`. Close stdin, drain stderr, require successful exit and complete marks, then validate the imported commit graph and pointer/object consistency before ref changes.

**Verify**: injected parser, writer, fast-import, marks, and validation failures leave original refs unchanged.

### Step 5: Commit refs atomically and restore the Working tree

Use one `git update-ref --stdin` transaction with expected old OIDs. If any expected ref changed, abort all updates. After commit, require checkout/reset of the original Worktree target to succeed. If post-commit restoration fails, perform one atomic rollback transaction using expected rewritten OIDs and report both primary and rollback outcomes.

**Verify**: concurrent-ref-change, checkout-failure, and rollback-failure tests prove there is never a silently partial ref set; any uncertain state is surfaced with exact recovery commands.

### Step 6: Test import/export byte identity and crash recovery

Cover include/exclude/above/fixup, inline blobs, paths with spaces/non-UTF-8 where supported, multiple refs, remote refs, repeated blobs, Crab-to-LFS, LFS-to-Crab, object maps, dirty Worktree rejection, cancellation, and process kill at every durable boundary. Import followed by export must reproduce original bytes and ref topology.

**Verify**: migration and broad LFS suites pass; crash fixtures resume or roll back without reusing unverified temporary state.

## Test plan

- Add subprocess integration fixtures beside existing migration tests; reuse repository helpers rather than mocking Git export syntax.
- Generate large content incrementally and record peak resident/spooled bytes.
- Assert remote request count is zero in local-first mode.
- Assert every ref transaction includes expected old OIDs.

## Acceptance criteria

- [ ] Migration memory is bounded independently of total exported history size.
- [ ] Large blob bodies are file-backed and SHA-256 hashed once into local LFS staging.
- [ ] Default migration has no remote side effects and never updates remote refs.
- [ ] Ref changes commit or roll back as one transaction; per-ref partial restoration is impossible.
- [ ] Checkout/restoration failure is returned, never ignored.
- [ ] Import→export reproduces bytes and selected ref topology exactly.
- [ ] Dry-run predicts temporary disk and rejects insufficient capacity before mutation.

## STOP conditions

- Phase 0 leaves local-versus-remote migration behavior unresolved.
- Git version in the support matrix cannot provide the required atomic `update-ref --stdin` transaction semantics.
- A parser design requires retaining all blob bodies or all serialized output in memory.
- Rollback cannot be conditioned on the rewritten ref OIDs.
- The Worktree is dirty or ref state changes after preflight.

## Maintenance notes

Reviewers should treat fast-export parsing limits, child-process deadlocks, ref transactions, and post-import validation as security/correctness boundaries. Temporary migration files must be recoverable, identity-bound, and cleaned only after successful finalization.
