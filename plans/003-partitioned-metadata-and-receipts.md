# Plan 003: Route metadata and origin receipts through bounded partitions

> **Executor instructions**: Build the reusable partition substrate and prove
> it independently of the full push path. Preserve ordered vector semantics and
> close every opened SlateDB on all exits. Update the plan index when complete.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crates/crab-metadata/src crates/crab-storage/src crab/src/metadata crab/src/metadata/metadb`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/002-layout-descriptor-and-dispatch.md`
- **Category**: perf, tech-debt
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

One repo file-index database and one bucket-global chunk-index database cannot
compact, open, or recover predictably at billions of keys. Partitioning must be
hash-deterministic, vectorized, bounded in open handles, and explicit about
partial success. The same substrate should host xorb origin-receipt heads so
push/fsck avoid one HEAD per xorb without inventing a second truth model.

## Current state

- `crab/src/metadata/metadb/mod.rs:213-243` owns exactly two lazy SlateDB slots;
  `file_index()` at `:359` and `chunk_index()` at `:373` open one each.
- `crab/src/metadata/metadb/transaction.rs:11-43` lowers every transaction into
  two fixed write batches. `MetaDb::commit` at `:411` writes them concurrently.
- `MetaDb::close_all` at `crab/src/metadata/metadb/mod.rs:694-719` explicitly
  closes both. Repository invariant: every opened SlateDB closes on every exit.
- `crab/src/metadata/metadb/stores/chunk_index.rs` already preserves ordered
  `get_batch` results and layers memory/SQLite/remote lookup.
- `crates/crab-metadata/src/receipts.rs:58-151` defines `OriginReceipt` with
  canonical object key, content/payload digest, size, ETag, version, and proof
  identity. Reuse this value contract.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Metadata tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata -p crab-storage --locked` | all pass |
| MetaDB tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked metadata::metadb -- --nocapture` | all pass |
| Lint | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo clippy -p crab-metadata -p crab-storage -p crab --locked -- -D warnings` | exit 0 |

## Scope

**In scope**:

- `crates/crab-metadata/src/partition.rs` (create)
- `crates/crab-metadata/src/transaction.rs`
- `crates/crab-metadata/src/value_codec.rs`
- `crates/crab-metadata/src/receipts.rs`
- `crates/crab-storage/src/layout.rs`
- `crab/src/metadata/metadb/partition_pool.rs` (create)
- `crab/src/metadata/metadb/transaction.rs`
- `crab/src/metadata/metadb/mod.rs`
- `crab/src/metadata/metadb/stores/{chunk_index,file_index}.rs`
- focused tests in these modules

**Out of scope**:

- Switching production add/push/read callers.
- Recipe object format.
- A distributed transaction across partitions.
- Eagerly opening all partitions or adding a metadata server.

## Git workflow

- Branch: `advisor/003-partitioned-metadata`
- Prefer commits for router/contract, handle pool, then stores/tests.
- Match conventional style; do not push without instruction.

## Steps

### Step 1: Define deterministic routing and keys

Add typed `MetadataKind`, `PartitionId`, and `MetadataPartitionRouter` contracts.
Route by leading content-hash bits from the validated descriptor; encode paths
with fixed-width hex IDs. Add typed path builders for
`{global_prefix}/partitioned1/metadb/chunk-index/{partition}`,
`{global_prefix}/partitioned1/metadb/xorb-receipts/{partition}`, and
`{repo}/metadata/partitioned1/file-index/{partition}`. Partition assignment
must be stable across platforms and independent of input order.

**Verify**: property tests show every random hash maps into range, identical
hashes map identically, boundary bits are correct, and routing does not depend
on file path/repo branch.

### Step 2: Implement a bounded handle pool

Create an async `PartitionHandlePool` with configured maximum open handles and
operation concurrency. Deduplicate concurrent opens for one partition. Evict
only idle handles, close evicted databases before reporting capacity, and add
an explicit consuming `close_all`. Cancellation and failed opens must not leak
capacity or poison retry. Avoid a synchronous mutex across `.await`.

**Verify**: multi-threaded Tokio tests cover concurrent same-partition open,
LRU eviction, transient open retry, normal/error/cancel cleanup, and assert
opened count equals closed count.

### Step 3: Generalize ordered multi-partition batches

Replace the fixed two-target lowering with operations addressed by metadata
kind plus partition. Group internally while retaining original indexes for
ordered results and partition-qualified errors. Writes are idempotent; if a
key already has a different immutable value, return corruption rather than
last-writer-wins. Partial writes return a receipt listing successful/failed
partitions so retry can safely replay.

**Verify**: tests randomize input order and failures, proving returned values
align with input and replay converges to the same values.

### Step 4: Add partitioned file/chunk stores and receipt head

Implement partitioned vector APIs without changing existing unified APIs.
Keep the current 40-byte chunk placement when the partition path carries the
layout version. Define a new file-index record only when Plan 005 needs recipe
roots. Add a partitioned xorb receipt head whose value is encoded
`OriginReceipt`; write it only after create/readback proof of the xorb. It is
rebuildable acceleration and cannot override missing/corrupt object bytes.

**Verify**: local object-store integration writes/reads across at least four
partitions, detects conflicting values, validates receipt identity, and closes
all handles after injected failures.

## Test plan

- Router property tests and fixed golden paths.
- Ordered batch with duplicates and empty input.
- Bounded concurrency and handle count.
- Partial write + idempotent replay.
- Conflicting immutable value rejection.
- Receipt schema/identity corruption.
- All close paths, including task cancellation.

## Done criteria

- [ ] No partition API returns results in grouped rather than caller order.
- [ ] Peak open handles and in-flight operations never exceed configuration.
- [ ] Every opened DB is observably closed on success, error, and cancellation.
- [ ] Partial writes are safe to replay and cannot move refs.
- [ ] Origin receipt head reuses `OriginReceipt` and remains rebuildable.
- [ ] Unified layout tests remain green; scoped lint/tests pass.

## STOP conditions

- The design requires cross-partition atomicity for correctness.
- A store must scan content-key ranges to classify a batch.
- Handle eviction can race an active borrower without a clear ownership proof.
- A proposed xorb catalog can claim existence without an `OriginReceipt` tied
  to the canonical object identity.

## Maintenance notes

Reviewers should focus on close/cancellation ownership, deterministic error
ordering, and replay semantics. Adding a partition kind later must reuse this
router/pool rather than creating a sibling handle lifecycle.
