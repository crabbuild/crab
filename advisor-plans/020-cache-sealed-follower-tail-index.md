# Plan 020: Reuse one verified index across follower-tail pages

> **Executor instructions**: Preserve all follower durability and corruption
> checks. This plan optimizes repeated reads of an already sealed lane; it does
> not change acknowledgement, fencing, or wire semantics. Run each gate and
> stop on any ambiguity about lane mutability. Update the index row when done.
>
> **Drift check (run first)**:
> `git diff --stat 892720ce6a6..HEAD -- crates/crab-cell-runtime/src/follower.rs crates/crab-cell-runtime/src/follower/tests.rs crates/crab-cell-runtime/src/node_log_transport.rs crates/crab-cell-runtime/src/node_log_recovery.rs crates/crab-http-server/src/cells/scheduler.rs`

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED — stale indexes must never hide gaps or serve unverified bytes
- **Depends on**: none
- **Category**: performance / recovery
- **Planned at**: commit `892720ce6a6`, 2026-09-19
- **Implementation status**: implemented and verified with the multi-page scan-count regression

## Why this matters

Recovery requests follower tails in bounded pages, but every page currently
enumerates every chunk and verifies every record in the sealed lane before
reading the requested range. Recovery cost therefore approaches page count
times lane size. A large source-loss recovery can spend most of its time
rescanning local SSD rather than transferring frames.

Sealed lanes are immutable through the runtime API. Build or load one verified
in-memory index under the existing per-lane lock, then reuse it for all pages.
Keep per-frame digest verification when materializing bytes so post-index disk
changes still fail closed.

## Current state

- `crates/crab-cell-runtime/src/follower.rs:25-26` maps each lane to a locked
  optional `LaneMemory`.
- `crates/crab-cell-runtime/src/follower.rs:268-290` locks the lane and calls
  `read_tail_sync` for every page.
- `crates/crab-cell-runtime/src/follower.rs:882-934` calls `scan_lane` before
  selecting a page.
- `crates/crab-cell-runtime/src/follower.rs:952+` enumerates and parses all
  chunk files into `StoredRecord` values.
- `seal_sync` at `crates/crab-cell-runtime/src/follower.rs:785-836` already runs
  under the same lane lock and makes future appends fail.
- `retire_sync` clears the in-memory state and removes chunks only after the
  authority-owned coverage check.

Use `crates/crab-cell-runtime/src/follower/tests.rs` as the test pattern. Do not
change `FollowerTailPage`, `TailRequest`, or peer wire encoding.

## Commands you will need

| Purpose | Command | Expected on success |
| --- | --- | --- |
| Follower unit tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail cargo test -p crab-cell-runtime follower::tests --locked` | exit 0 |
| Recovery tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail cargo test -p crab-cell-runtime --test actor source_loss --locked` | exit 0 or exact current matching filters pass |
| Transport tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail cargo test -p crab-cell-runtime node_log_transport --locked` | exit 0 |
| LTX proof | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail cargo test -p crab-ltx --features replica --locked` | exit 0 |
| Full runtime | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail cargo test -p crab-cell-runtime --release --locked` | exit 0 |
| Quality | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail cargo clippy -p crab-cell-runtime --all-targets --all-features --locked -- -D warnings && cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- `crates/crab-cell-runtime/src/follower.rs`
- `crates/crab-cell-runtime/src/follower/tests.rs`
- Recovery/transport tests only where required to prove pagination behavior

**Out of scope**:

- New persistent follower file formats or compatibility readers
- Changing chunk rotation, append acknowledgement, seal/retire markers, or retention
- Removing the per-frame digest recheck at page materialization
- Caching unsealed lanes for recovery reads
- HTTP protocol or page-size changes
- General filesystem abstraction work

## Git workflow

- Branch: `codex/020-follower-tail-index`
- Commit style: `perf(cell-runtime): reuse sealed follower tail index`
- Do not push or open a PR unless instructed.

## Steps

### Step 1: Add a multi-page scan-count regression

Create a sealed lane containing enough frames to require at least three pages.
Add a test-only scan observer or counter at the narrow `scan_lane` boundary;
do not introduce a production metrics label keyed by lane or Cell ID.

Read all pages using their returned `next_sequence`. Assert:

- the complete ordered frame sequence is returned exactly once;
- the lane scan count is one for the complete paginated read;
- each page respects byte/frame bounds, allowing the documented single-large-frame exception.

Open a new `FollowerStore` over the same directory and repeat. The restarted
store may scan once on its first page, but not again for later pages.

**Verify**: the scan-count assertion fails on current code with one scan per page.

### Step 2: Extend lane memory with a sealed record index

Add an optional sealed index to the lane-owned state. The index contains the
existing `StoredRecord` metadata keyed by sequence; do not duplicate frame
bytes. Populate it:

- during seal, from one authoritative `scan_lane` result; or
- lazily on the first page after restart, after validating the sealed marker.

Reuse the index in `read_tail_sync`. Validate requested base/durable bounds and
the seal watermark from the cached index. Keep the existing lane mutex held
while selecting/materializing the page so retire cannot race it.

Appending must invalidate any cached sealed index before modification, although
the normal API already rejects append after seal. Retire must clear it.

**Verify**: multi-page and restart scan-count tests pass.

### Step 3: Preserve corruption detection

Add tests for:

- chunk deletion after indexing;
- frame mutation after indexing;
- seal watermark disagreement;
- a sequence gap discovered during the initial scan;
- concurrent page read and retirement serialized by the lane lock.

Deletion must return an I/O-derived error. Mutation must fail the existing
BLAKE3 comparison. Do not silently rescan and accept changed bytes.

**Verify**: focused follower tests pass and each corruption case fails closed.

### Step 4: Measure the algorithmic improvement

Add an ignored/manual qualification test or extend plan 024's harness to create
a lane with at least 100,000 frames and read it in bounded pages. Record scan
count, pages, total bytes, wall time, and peak RSS. The machine-checkable local
criterion is scan count, not a laptop-specific latency.

**Verify**: scan count is one per store lifetime; total recovered bytes and
digest match the source lane.

### Step 5: Run broad recovery proof

Run full runtime, replica, Clippy, format, and diff checks. If server recovery
types changed unexpectedly, build the repository frontend and run the full
server library suite as an affected consumer.

## Done criteria

- [x] Multi-page reads scan a sealed lane at most once per `FollowerStore` lifetime.
- [x] Restart performs at most one new scan and returns identical pages.
- [x] Page byte/frame bounds and wire shapes are unchanged.
- [x] Deleted, mutated, gapped, or watermark-inconsistent storage fails closed.
- [x] Retire and read remain serialized; retained disk accounting is unchanged.
- [x] Runtime, LTX replica, recovery, Clippy, and format gates pass.

## STOP conditions

- A lane can legitimately append after being visible as sealed.
- Correctness would require trusting index metadata after frame bytes change.
- The implementation requires a new on-disk index or compatibility reader.
- Memory for the index cannot fit inside the existing bounded lane/resource model.

## Maintenance notes

The cached index is derived state, not authority. Any new operation that mutates
lane chunks must invalidate it under the same lock. If future workloads make
the in-memory record index itself too large, design a versioned persistent
index separately with migration and crash-consistency proof; do not quietly
weaken verification here.
