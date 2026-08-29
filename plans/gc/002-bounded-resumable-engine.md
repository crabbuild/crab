# Plan 002: Make current GC bounded, journaled, and resumable

> **Executor instructions**: Follow this plan in order. Run every verification
> gate. Stop on a listed STOP condition; do not invent a fallback. Update the
> status row in `plans/gc/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat ff7792a4..HEAD -- crab/src/cmd/gc crab/src/main.rs crates/crab-storage/src/store.rs crates/crab-metadata/src crab/src/core/output crab/docs packages/web/content/docs/cli`
> Compare the current-state excerpts below against live code if anything moved.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/gc/001-writer-admission.md`
- **Category**: perf, security
- **Planned at**: commit `ff7792a4`, 2026-08-22

## Why this matters

Current repo and bucket GC materialize complete object listings and complete
reachability sets in memory. Bucket delete also loses durable accounting when a
request fails after sibling deletes have succeeded. This plan creates one
bounded mark/join/sweep engine with a sealed dry-run journal, explicit execute,
idempotent outcomes, cancellation, and restart after process or provider
failure.

## Current state

- `crab/src/cmd/gc/mod.rs:501-541` launches five raw recursive LIST streams,
  `try_collect`s each complete prefix, flattens them into one `Vec`, and always
  returns `failed_prefixes: Vec::new()` on success. A LIST error aborts; the
  advertised partial-enumeration state is not produced by this path.
- `crab/src/cmd/gc/mod.rs:564-686` accepts `Vec<ObjectMeta>` and `HashSet`
  roots, computes all candidates, then deletes in memory-sized batches.
- `crab/src/cmd/gc/bucket.rs:169-327` materializes global shard/xorb listings,
  referenced sets, and candidates before deletion.
- `crab/src/cmd/gc/bucket.rs:893-932` uses
  `buffer_unordered(...).try_collect()`: the first error returns even though
  other deletes may already have succeeded, and no durable outcome is written.
- `crates/crab-storage/src/store.rs:1148-1177` exposes `list_prefix` and
  `list_prefix_bounded`; both buffer their returned collection. GC also calls
  `Store::inner().list` directly, bypassing the Store error/retry boundary.
- `Cargo.lock` pins `object_store 0.14.1`. Before relying on retry, pagination,
  ETag/version, or cancellation behavior, read that version's source and the
  provider implementations. Do not infer a resumable cursor from the trait.
- `crab/src/cmd/gc/mod.rs:460-487` gives production
  `StoreObjectDeleter::reconcile_manifest` a no-op body even though comments and
  summary fields imply post-delete manifest reconciliation. The manifest is a
  root, not a deletion list; the new engine must remove this false phase.

The target pipeline is:

```text
validated roots -> partitioned external mark files
candidate source -> normalized partition rows
sorted merge/join -> sealed dry-run journal
explicit execute -> admit/fence/re-mark batch -> delete -> durable outcomes
```

No stage may hold a complete namespace, complete repository, or complete file
closure in a `Vec`, `HashSet`, or `BTreeMap`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Storage stream tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-storage --locked gc_list_stream` | all matching tests pass |
| Journal tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata --locked gc_journal` | all matching tests pass |
| Engine tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_engine` | all matching tests pass |
| Existing GC tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib cmd::gc --locked` | all GC tests pass |
| Format | `cargo fmt --all -- --check` | exit 0 |

Use the dedicated external target directory for every Cargo invocation.

## Scope

**In scope**:

- `crates/crab-storage/src/store.rs` and tests
- `crates/crab-metadata/src/gc_journal.rs` (create), `lib.rs`, and errors
- `crab/src/cmd/gc/engine/` (create)
- `crab/src/cmd/gc/{mod,bucket,parallel_enum}.rs`
- `crab/src/main.rs`
- GC structured output and error catalog entries
- an explicit GC scratch-directory CLI/configuration surface with a hard byte
  budget
- `crab/docs/architecture/object-storage-layout.md`
- GC/recovery docs under `packages/web/content/docs/cli/`

**Out of scope**:

- Provider inventory formats; standalone GC Plan 004 adds candidate-source
  adapters.
- Shard closure optimization; standalone GC Plan 003 removes full shard
  downloads.
- History pruning policy changes.
- Direct provider batch-delete services or S3 Batch Operations.
- A local SQLite authority. Scratch files are rebuildable; the sealed object
  journal is the durable run contract.

## Git workflow

- Branch: `gc/002-bounded-resumable-engine`
- Commits: streaming primitives, journal contract, repo engine, bucket engine,
  CLI/docs.
- Do not push or execute live bucket deletion without explicit instruction.

## Steps

### Step 1: Add a cancellation-aware candidate stream boundary

Add a `Store` listing API that yields normalized `ObjectMeta` incrementally,
maps provider errors through `crab-storage`, checks cancellation between items,
and exposes ETag/version when the dependency supplies it. Inspect
`object_store 0.14.1` source first. If its list stream already retries provider
pages, document and test that contract. If it does not expose a continuation
token, a retry may restart only the current fixed prefix/partition and must
deduplicate through external scratch; never pretend an opaque stream can resume
mid-page.

The API must accept an item/byte channel budget and never return a complete
`Vec`. Keep `list_prefix` for existing small callers; switch only GC here.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-storage --locked gc_list_stream`
→ bounded-buffer, cancellation, transient-error, duplicate-on-restart, and
terminal-error tests pass.

### Step 2: Define the durable run and journal contract

In `crates/crab-metadata/src/gc_journal.rs`, define versioned records for:

- run identity, scope roots, layout version, policy, start time, and state;
- authoritative root identities: manifest/history/workflow/registry/coordinator
  generations and admission epoch for the current Unified storage contract;
- candidate-source identities and partition set;
- candidate key, size, last-modified, provider ETag/version when available,
  reason, and grace decision;
- per-attempt outcome: deleted, already absent, retained after re-mark, failed
  retryable, or failed terminal;
- chained BLAKE3 digests and a final sealed manifest containing row/count/byte
  totals per partition.

Store immutable journal segments create-only under canonical repo/global GC run
paths. Seal through CAS only after every partition is complete. Execution pins
the sealed manifest object identity. Outcomes are separate append-only segments
so a retry never rewrites the plan. Parsing is strict: duplicate partition,
missing segment, count mismatch, hash mismatch, unknown version, or scope drift
invalidates execution.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata --locked gc_journal`
→ serialization, corruption, truncation, duplicate, deterministic digest, and
resume fixtures pass.

### Step 3: Build an external-memory mark/join engine

Create private engine modules for root streaming, candidate normalization,
partition spill, external sort/dedup, merge join, journal seal, and scratch
budget accounting. Scratch resides under a run-specific directory inside an
operator-selected scratch root; every filename derives from a validated
run/partition ID, not an object key. Large inventory mode requires an explicit
scratch root and never falls back to the checkout, local Cargo target, or an
unknown small system temp volume. Enforce configured RAM bytes, channel
entries, open files, scratch bytes, and partition concurrency. On budget
exhaustion, stop without sealing.

Reachability emits normalized keys into fixed hash partitions. Candidate rows
use the same partition function. A sorted merge emits only unreferenced rows
outside grace and quarantine. Repeated keys collapse deterministically while
preserving the strongest provider identity. All candidate-source or root
errors make the run `invalid`; partial enumeration is never executable.

Replace the current misleading no-op reconciliation abstraction. Successful
deletion does not mutate manifests; root revalidation and durable outcomes are
the real postcondition. If shipped structured output compatibility requires a
schema bump, bump the output version and document it rather than keeping a
field with false semantics.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_engine_mark_join`
→ a synthetic graph larger than the RAM budget completes with stable digest;
injected root/candidate error never produces an executable journal.

### Step 4: Move repo and bucket GC onto the same engine

Implement candidate adapters for the five repo prefixes and 256 global
shard/xorb partitions. Adaptive recursive LIST may remain for small dry-run
diagnostics, but destructive plans must have fixed restartable partitions so a
failed partition is identifiable. Convert current manifest, every validated
historical manifest, ref journal, workflow/artifact roots, coordinator proof,
and ref-registry state into root streams. Keep repo-local and shared-object
ownership separate.

Delete old whole-namespace collection paths after both scopes use the engine.
Do not retain a second immediate-delete implementation. Dry-run and execute
must differ only after journal sealing.

**Verify**: run `gc_engine_repo`, `gc_engine_bucket`, and the existing GC test
commands → current reachability/grace/history/workflow/registry behavior remains
covered, with bounded peak buffers asserted.

### Step 5: Execute and resume under bounded admission fences

Execution requires an explicit sealed run ID. Before each bounded batch:

1. acquire the standalone GC Plan 001 sweep guard;
2. validate scope, journal identity, admission epoch, and all roots;
3. re-mark the candidate batch and re-check grace/quarantine;
4. issue idempotent deletes with bounded concurrency;
5. durably append every outcome before releasing the batch checkpoint;
6. release the guard and honor cancellation before the next batch.

Collect every request result; never let `try_collect` discard sibling success
accounting. Treat NotFound as idempotent success. Retry only classified
transient errors through `Store`. A terminal error stops new batches after
recording all in-flight results; resume retries unfinished/retryable keys and
never re-counts completed ones.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_engine_resume`
→ fault injection at every seal/checkpoint/delete boundary resumes to exactly
one terminal outcome per candidate and never deletes a newly rooted object.

### Step 6: Make planning the default CLI behavior

Make plain `crab gc` produce and seal a preview journal. Add explicit scratch
root and scratch-byte-budget options; show required/available capacity before
enumeration and keep the chosen root out of the durable object-key contract.
Keep shipped
`--dry-run` as an explicit preview request, but remove the old implicit
immediate-delete path. Add one explicit execute form that requires the run ID
and confirmation; structured output reports run ID, source/root identities,
candidate counts/bytes, progress, failures, retained-after-recheck, and resume
instructions. Force is allowed only while planning and is pinned in the sealed
policy; execute cannot strengthen it later.

Update CLI, recovery, and operations docs. Include cleanup policy for completed,
invalid, and abandoned journals; journal cleanup itself respects retention and
never removes an active run.

**Verify**: CLI tests prove plain command and `--dry-run` make no deletions,
execute refuses an unsealed/stale/wrong-scope run, cancellation prints a usable
resume command, and docs match help. Run format and all GC tests.

## Test plan

- Stream backpressure, cancellation, provider retry/restart, and duplicate rows.
- Journal round-trip plus every corrupt/incomplete shape.
- Property tests: reachable, historical, active-writer, quarantined, and grace
  objects never appear in an executable candidate set.
- External-memory stress where input cardinality exceeds RAM capacity.
- Deterministic digest across input ordering and partition concurrency.
- Fault injection before/after every durable boundary and each delete result.
- Existing repo/bucket GC behavior migrated without a parallel old path.

## Done criteria

- [ ] No GC production path collects a complete namespace or root closure in
  process memory.
- [ ] Enumeration/root failure cannot produce an executable partial plan.
- [ ] Plain GC is plan-only; deletion requires a sealed run ID.
- [ ] Every attempted delete has one durable terminal or retryable outcome.
- [ ] Resume is idempotent after process death, cancellation, and provider error.
- [ ] The no-op manifest reconciliation phase and immediate-delete path are gone.
- [ ] Targeted tests, existing GC tests, format, and `git diff --check` pass.

## STOP conditions

- `object_store 0.14.1` list semantics are assumed rather than read and tested.
- A design requires an unbounded in-memory set, unbounded channel, or one file
  descriptor per partition.
- Provider identity is insufficient for the promised replacement/race check;
  rely on standalone GC Plan 001 fencing and state the limitation instead of guessing.
- A root or candidate error is proposed as a warning while deletion continues.
- Execution could proceed without a sealed journal or after root/scope drift.
- Scratch capacity or `/Volumes/Workspace` is unavailable.

## Maintenance notes

The journal format is a durable operator and cross-version contract. Reviewers
should focus on seal validation, root invalidation, scratch bounds, outcome
durability, and any collection hidden behind a nominally streaming API.
