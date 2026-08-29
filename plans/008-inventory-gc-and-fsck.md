# Plan 008: Make GC and fsck inventory-driven and resumable

> **Executor instructions**: Introduce strict provider inventory ingestion and
> an external-memory mark/sweep engine. Default to dry-run. Do not use recursive
> LIST as the PB deletion control plane and never run bucket-wide GC in testing.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crab/src/cmd/gc crab/src/cmd/fsck.rs crab/src/cost/inventory crates/crab-metadata/src crab/docs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: Plans 003, 005, and 006
- **Category**: security, perf
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

Current GC enumerates objects with LIST, which is costly and operationally
fragile at millions of xorbs/recipes. Existing cost-report parsers summarize
CSV and silently skip malformed rows or turn invalid sizes into zero, so they
cannot authorize deletion. GC needs strict provider report validation,
external-memory reachability, grace rules, and a resumable evidence journal.

## Current state

- `crab/src/cmd/gc/mod.rs:1-13` and `crab/src/cmd/gc/bucket.rs:1-18` describe
  LIST-based mark/sweep; `parallel_enum.rs` batches listing requests.
- `crab/src/cost/inventory/report/s3.rs:37-77` naïvely splits CSV, skips malformed
  lines, and uses `size.parse().unwrap_or(0)`.
- GCS docs call the source Parquet while `gcs.rs:26-92` parses CSV and records
  schema `parquet`; Azure has the same silent-skip/zero-size shape.
- These parsers return aggregate `Inventory`, not a strict candidate row stream.
- Safety invariants: never delete referenced xorbs or objects inside the grace
  period; metadata can be ahead of refs and must be protected through grace.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Inventory/GC tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked` | all library tests pass |
| Property tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked gc_never -- --nocapture` | all matching tests pass |

## Scope

**In scope**:

- `crab/src/cost/inventory/report/{mod,s3,gcs,azure}.rs`
- `crab/Cargo.toml` and `Cargo.lock` only if a reviewed format-reader dependency
  is required
- `crab/src/cmd/gc/` except unrelated legacy cleanup
- `crab/src/cmd/fsck.rs`
- `crab/src/cmd/fsck_store.rs`
- `crab/src/cmd/gc/inventory.rs` (create)
- `crab/src/cmd/gc/journal.rs` (create)
- `crab/src/metadata/metadb/stores/chunk_index.rs`
- `crab/src/metadata/metadb/stores/file_index.rs`
- `crates/crab-metadata/src/recipe_tree.rs`
- provider setup/runbook docs under `crab/docs/`

**Out of scope**:

- Editing baseline/ignore files to silence tests.
- A live `crab gc --scope=bucket` run.
- Treating cache contents as roots.
- Deleting canonical v1 data. Pre-cutover development repositories are reset
  explicitly by Plan 010, outside normal GC.

## Git workflow

- Branch: `advisor/008-inventory-gc-fsck`
- Commits: strict readers, reachability engine, journal/delete path, docs/tests.

## Steps

### Step 1: Define strict inventory row and report contracts

Create provider-neutral rows containing exact key, size, last-modified,
ETag/version when available, deletion marker/state, and report identity. Provider
readers must use real CSV/Parquet/ORC libraries appropriate to documented
formats, validate declared columns/types, and fail on malformed relevant rows.
Validate provider report manifest, bucket/scope, completion marker, generation,
age, and all expected data files before yielding any candidate.

**Verify**: fixtures for S3/GCS/Azure cover valid reports plus missing shard,
wrong scope, stale/incomplete manifest, quoted CSV, invalid size/date, unknown
schema, deletion markers, and truncated Parquet/ORC. Every malformed relevant
row fails the run.

### Step 2: Stream authoritative roots and reachability

Snapshot the unified manifest root and partition generations. Traverse refs,
Git packs, file heads, recipe trees, chunk placements, xorb receipts, active
push registrations, migration roots, and grace-period orphans through bounded
iterators. Partition/sort hashes to external scratch storage with explicit disk
budget; never build a PB-wide in-memory set.

**Verify**: a synthetic graph larger than the configured RAM budget completes,
and property tests assert every reachable object is marked despite duplicates,
partial metadata ahead of refs, and active push leases.

### Step 3: Produce a dry-run candidate journal

Join strict inventory rows against reachability by partition. Candidate
eligibility requires unreferenced, outside grace, report fresh/complete,
matching scope, not active, and not protected by migration retention. Write an
append-only journal with run ID, layout descriptor digest, root/generation
snapshot, inventory manifests, candidate identity, reason, and a chained BLAKE3
digest. Store the journal create-only and pin its object-store ETag/version in
the execute request. If operator cryptographic signatures are a product
requirement, STOP for a key-management decision instead of inventing one.
Dry-run is default.

**Verify**: rerunning with identical inputs produces the same candidate set and
digest; changed roots/generations invalidate execution rather than reusing it.

### Step 4: Execute and resume safely

Require an explicit execute flag and validated dry-run journal. Before each
bounded delete batch, re-check run/root/generation/grace/active-push guards.
Record per-object outcome idempotently; retry transient failures and stop on
scope/corruption drift. Advance per-partition `gc_generation` only after that
partition completes and its journal is durable.

**Verify**: fault injection at every journal/delete/generation boundary resumes
without double-accounting or deleting reachable objects.

### Step 5: Share the engine with fsck

Fsck consumes the same strict inventory and reachability streams to report
missing objects, receipt mismatches, recipe coverage errors, and catalog/index
drift. Repair remains a separate explicit action; fsck does not synthesize proof
or silently rewrite metadata.

**Verify**: corruption fixtures produce deterministic qualified errors and no
writes in report-only mode.

## Test plan

- Strict S3, GCS, and Azure manifest/row fixtures for every accepted format.
- Property tests that reachable, grace-period, active-push, and migration-held
  objects are never candidates.
- External-memory stress above the configured RAM budget.
- Deterministic dry-run digest and stale-root invalidation.
- Crash/retry at each journal, delete-batch, and generation boundary.
- Fsck report-only corruption cases proving zero writes.

## Done criteria

- [ ] No parse error is skipped or coerced to size zero for deletion input.
- [ ] Provider report completeness/scope/freshness is verified before marking.
- [ ] PB synthetic mark/sweep stays within declared RAM/disk budgets.
- [ ] Default is dry-run; execution requires a validated journal.
- [ ] Interrupted deletion resumes safely; live/grace/active objects survive.
- [ ] PB mode performs no recursive object LIST.
- [ ] Provider docs and scoped tests/lint/format pass.

## STOP conditions

- Provider report format/default semantics have not been verified from official
  documentation/source.
- Scratch capacity is insufficient for the declared external-memory budget.
- A root/generation changes without a defined invalidation/restart rule.
- Any test or operator command would run bucket-wide deletion.
- A new report-reader dependency is required but its license, maintenance, and
  `Cargo.lock` diff have not been reviewed.

## Maintenance notes

Inventory delay extends retention; it never justifies weaker completeness
checks. Reviewers should treat parser leniency, journal replay, and root snapshot
validation as destructive-action security boundaries.
