# Plan 006: Classify while streaming and commit PB pushes safely

> **Executor instructions**: Complete the first end-to-end Partitioned1 write
> path. Reuse canonical indexed staging, partitioned metadata, recipe trees, and
> the existing unified manifest CAS. Do not optimize away correctness checks.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crab/src/cmd/add.rs crab/src/cmd/add_push_plan.rs crab/src/git/push.rs crates/crab-staging/src crates/crab-coordination/src crab/tests`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/003-partitioned-metadata-and-receipts.md`,
  `plans/004-bounded-local-recipes.md`, and
  `plans/005-remote-recipe-tree.md`
- **Category**: perf, migration
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

Current streaming can write prepared xorbs directly, but remote classification
happens after the full file has been staged/planned. Cross-file or remote
duplicates may therefore be packed locally before lookup. This phase batches
classification during the read, records durable proofs for hits, packs misses
once, and preserves the rule that the unified manifest moves only after every
required object and metadata record is durable.

## Current state

- `crates/crab-staging/src/stream.rs:973-1020` already skips raw staging when a
  prepared-xorb builder exists.
- `crates/crab-staging/src/add_push_plan.rs:1670-1705` tests that direct staging
  retains no raw segment copy.
- `crab/src/cmd/add.rs:1217-1273` performs planning after staging.
- `crab/src/cmd/add_push_plan.rs:151-200` batches remote chunk-index lookup; on
  unavailable/wrong/error response it safely classifies all terms as new.
- `crab/src/git/push.rs:6467-6475` documents why unified manifest CAS replaced
  the older split publication. Per-ref lock ownership spans upload/commit at
  `:10700-10753`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Staging tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked` | all pass |
| Push/add tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked` | all library tests pass |
| Transcript | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --test remote_helper_transcript --locked` | all pass |

## Scope

**In scope**:

- `crates/crab-staging/src/stream.rs`
- `crates/crab-staging/src/add_push_plan.rs`
- `crates/crab-staging/src/lib.rs`
- `crab/src/cmd/add.rs`
- `crab/src/cmd/add_push_plan.rs`
- `crab/src/git/push.rs`
- `crab/src/coordination/pipelined_commit.rs`
- `crates/crab-coordination/src/write_coordinator.rs` only if it owns the
  immutable-before-manifest ordering
- `crab/tests/remote_helper_transcript.rs`
- focused crash/E2E tests

**Out of scope**:

- Cache-service internals (Plan 007).
- Ref-map externalization (Plan 009).
- Silent reconstruction from corrupt staged state.
- Cross-partition atomic transactions.

## Git workflow

- Branch: `advisor/006-streaming-dedup-push`
- Commit by safe vertical slices: classifier interface/tests, add integration,
  push commit/readback/fault tests.
- Do not push without instruction.

## Steps

### Step 1: Define the bounded classifier contract

Introduce an `ExistingChunkLookup` stream adapter accepting bounded batches and
returning one proof result per input occurrence in order. A valid hit contains
the chunk placement plus origin receipt/proof generation needed by push. A
miss feeds bytes to the existing prepared-xorb writer. Lookup unavailable,
timeout, malformed cardinality, or stale proof must conservatively become
`UnknownPackLocally`, never data loss and never add failure unless cancellation
or local durability fails.

**Verify**: tests cover mixed hits/misses, repeats across batches/files, stale
proof, timeout/error fallback, cancellation, and bounded in-flight bytes.

### Step 2: Integrate classification into the one-pass add stream

Buffer payload only until one classification batch resolves. Record hit terms
and proof IDs in Plan 004 pages; send miss bytes directly to the existing xorb
packer. Atomically seal pages, prepared xorbs/residual bytes, and indexed plan
before publishing the pointer/index entry. Do not keep a complete-file vector
or raw duplicate segment.

**Verify**: duplicate-add fixture reports zero segment bytes and zero prepared
xorb bytes for proven hits; all-new input stores each payload byte once apart
from bounded residual/format overhead; lookup failure still produces a valid
local plan.

### Step 3: Make push consume canonical staged terms

Push iterates the sealed staged recipe. Revalidate stale remote proofs; upload
or create-verify each prepared xorb; write its `OriginReceipt` head; write
partitioned chunk placements; build/write recipe tree; write the partitioned
file head; upload Git packs. Every write is idempotent and immutable conflicts
are corruption. Only then execute the existing single manifest CAS while the
existing ref lock is held. Retire staging after successful CAS/readback.

**Verify**: transcript/integration tests observe one manifest CAS root, no
separate ref CAS, and exact clone/hydrate bytes.

### Step 4: Inject every crash boundary

Cover crash/cancellation after prepared xorb, origin receipt, each chunk-index
partition group, recipe node/root, file head, Git pack, before manifest CAS,
after manifest CAS, and during staging retirement. Before CAS, refs remain old;
retry converges. After CAS, operation reports/reconciles success and data is
readable. Orphans remain protected by grace-period policy for Plan 008.

**Verify**: table-driven fault suite passes for every named boundary and asserts
no ref points to unavailable bytes/metadata.

## Test plan

- Bounded streaming classification, duplicate and repeated chunks.
- Safe pack-local fallback for every remote/cache error shape.
- Immutable conflict rejection and idempotent replay.
- Multi-ref atomic transcript and same-ref lock serialization.
- Fresh clone/hydrate byte identity on a real S3-compatible store.
- All MetaDB partition handles close on normal/error/cancel exits.

## Done criteria

- [ ] Partitioned1 add/push/read is a Level 3 E2E vertical slice.
- [ ] Proven duplicates create no local payload copy or remote xorb upload.
- [ ] Unknowns are packed once and remain recoverable offline until push.
- [ ] Refs never move before all required objects and metadata are durable.
- [ ] Fault suite covers every listed boundary and retry converges.
- [ ] Unified layout behavior and remote-helper transcript remain green.

## STOP conditions

- A valid push would require trusting cache/local proof without origin identity.
- A proposed retry overwrites a conflicting immutable value.
- Any branch moves refs before recipe/file/chunk/origin metadata durability.
- Supporting both layouts duplicates the full push pipeline rather than
  dispatching narrow stores/codecs.

## Maintenance notes

The conservative lookup-failure path is intentional correctness behavior, but
must emit metrics distinguishing performance fallback from corruption. It is
not permission to add more fallback readers elsewhere.
