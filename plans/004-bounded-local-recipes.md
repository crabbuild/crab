# Plan 004: Make local recipes bounded and canonical

> **Executor instructions**: Replace complete-file chunk vectors at the local
> staging boundary with a paged sequence. Keep current small-file behavior only
> as a bounded inline optimization. Do not add a second staging authority.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crates/crab-diff/src/chunk_sequence.rs crates/crab-staging/src crab/src/cmd/add.rs crab/src/cmd/add_push_plan.rs crab/src/git/clean.rs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-pb-contract-and-baselines.md`
- **Category**: perf, tech-debt
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

The current stream reads payload bytes incrementally but retains one entry per
chunk in several complete-file vectors. A multi-TB file can therefore exhaust
memory even before remote metadata is considered. The correct boundary is a
durable, paged, integrity-sealed sequence inside the existing indexed staging
store, exposed through bounded iterators.

## Current state

- `crates/crab-staging/src/stream.rs:255-264` returns `chunk_pairs: Vec`, a
  `FileRecipe`, and prepared xorbs for the full file.
- `crates/crab-staging/src/recipe.rs:43-82` stores every hash/size pair in
  `RecipeRecorder.chunks: Vec`.
- `crates/crab-diff/src/chunk_sequence.rs:33-40` stores `spans: Vec<ChunkSpan>`.
- `crab/src/cmd/add.rs:1254-1260` converts all staged entries into borrowed
  complete chunk slices for fallback planning.
- Existing indexed push-plan authority already exists in
  `crates/crab-staging/src/add_push_plan.rs:379-399` and promotion/loading in
  `crates/crab-staging/src/lib.rs:2344-2397`. Extend it; do not create
  `.crab/staging-v2`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Staging/diff tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-diff -p crab-staging --locked` | all pass |
| Add tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked cmd::add -- --nocapture` | all pass |
| Search | `rg -n "chunk_pairs: Vec|chunks: Vec<\(MerkleHash, u64\)>" crates/crab-staging/src crab/src/cmd/add.rs` | no production complete-file owner remains |

## Scope

**In scope**:

- `crates/crab-staging/src/recipe.rs`
- `crates/crab-staging/src/recipe_pages.rs` (create)
- `crates/crab-staging/src/stream.rs`
- `crates/crab-staging/src/index.rs`
- `crates/crab-staging/src/lib.rs`
- `crates/crab-staging/src/add_push_plan.rs`
- `crates/crab-diff/src/chunk_sequence.rs` only for iterator-friendly APIs
- `crab/src/cmd/add.rs`
- `crab/src/cmd/add_push_plan.rs`
- focused tests

**Out of scope**:

- Remote recipe trees and file-index records.
- Remote dedup during the file read; Plan 006 owns it.
- A new staging directory/schema authority.
- Loading all pages to preserve an old convenience API.

## Git workflow

- Branch: `advisor/004-bounded-local-recipes`
- Commits: page contract/storage, streaming integration, caller cleanup/tests.
- Do not push without instruction.

## Steps

### Step 1: Define paged local recipe records

Add an append-only page format keyed by staging batch/file identity. Each page
contains a bounded number/byte size of ordered `(chunk_hash, size)` terms plus
start chunk, start byte, counts, and a page digest. A sealed root records file
hash, file size, policy ID, total counts, ordered page digests, and an
incremental recipe digest. Publish the root atomically only after pages and any
referenced local payloads are durable. Unknown versions and non-contiguous
coverage are corruption errors.

**Verify**: round-trip and corruption tests cover empty, one-page, multi-page,
truncated, reordered, duplicated, and overflowing sequences.

### Step 2: Replace `RecipeRecorder` with a spilling recorder

Give the recorder a byte/entry budget and page sink. `record` may flush a page
but never retain more than the configured page plus small hashing state. `seal`
returns a root handle, not a materialized `FileRecipe`. Keep a small inline
page only below a fixed tested limit; it must use the same iterator contract.

**Verify**: an instrumented test records millions of synthetic terms and
asserts maximum buffered entries never exceeds the configured limit.

### Step 3: Make staging consumers iterate pages

Change `StreamStageResult`, indexed plan persistence, status/recovery, and add
planning to carry the root handle and open bounded page iterators. Any consumer
that needs batching must take at most configured entries/bytes. Remove the
fallback construction of `Vec<AddPlanFile>` with whole-file chunk slices.

**Verify**: staging/add tests pass and the search command finds no production
complete-file owner in the scoped staging/add path.

### Step 4: Prove lifecycle and crash behavior

On cancellation or failed publication, rollback unsealed pages with the same
batch lease as segments/prepared xorbs. Sealed roots remain available to
status/push/recovery. Cleanup must not delete a page still referenced by a
published root.

**Verify**: fault tests after page flush, payload seal, root seal, and pointer
publication yield either no visible staged file or one fully iterable recipe.

## Test plan

- Property test: random sequences round-trip with contiguous exact coverage.
- Peak-buffer assertion independent of total terms.
- Incremental digest equals a small in-memory reference implementation.
- Cancellation and rollback at every publication boundary.
- Status, clean, retry, and indexed plan adoption use the same root.

## Done criteria

- [ ] No complete-file chunk `Vec` crosses stream → staging → add planning.
- [ ] The root is the canonical indexed staging recipe for small and large files.
- [ ] Memory is bounded by configured page/batch size in an instrumented test.
- [ ] Crash recovery cannot publish a root with missing pages/payloads.
- [ ] Scoped format/tests/lint pass.

## STOP conditions

- SQLite/blob limits or current lease schema cannot make page/root publication
  atomic without changing another owner; report the required boundary.
- Any caller insists on rebuilding the complete vector.
- A separate `.crab/staging-v2` path appears necessary.
- The new digest differs from current recipe identity for small files without
  an explicit migration/format decision.

## Maintenance notes

The iterator is a foundational API for Plan 005/006. Reviewers should scrutinize
page durability ordering, digest domain separation, and cleanup ownership more
than the inline optimization.
