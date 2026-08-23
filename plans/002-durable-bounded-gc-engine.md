# Plan 002: Replace the in-memory sweep with a durable bounded run engine

> **Executor instructions**: Follow the steps in order and run each gate.
> Stop and report on any STOP condition; do not preserve the old delete path as
> a fallback. Update `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat b738f3b2..HEAD -- crab/src/cmd/gc crab/src/main.rs crab/src/core/config.rs crates/crab-storage/src/layout.rs crates/crab-metadata/src/manifest_store.rs crates/crab-workflow/src/gc.rs`
> Re-read the current excerpts before editing if the diff is non-empty.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-close-writer-gc-fence.md`
- **Category**: perf
- **Planned at**: commit `b738f3b2`, 2026-08-22

## Delivery status

Partial implementation in this branch: object-store journals, bounded delete
batches, root identities, repo and bucket prefix/root streaming, explicit
resume, durable file-index reconciliation markers, streamed historical-manifest
reads, pack-segment visitors, and partitioned repo/bucket mark sets with
bounded buffers are wired. Destructive repo and bucket paths no longer collect
the full candidate namespace; preview/repair paths retain collections. The
repository snapshot/ref-journal materializer and workflow artifact registry
still have bounded-but-materialized joins. Planning replay is safe but
restarts source enumeration instead of persisting provider cursors.

## Why this matters

The original repo and bucket paths collected all listed candidates and
reachability roots into `Vec`/`HashSet` values. The delete loop was chunked, but
the plan and progress were process-memory-only; a crash after half the deletes
left no authoritative outcome. The destructive paths now use object-store
journals and partitioned marks as the source of truth. Remaining joins are
explicitly bounded and fail closed; they are not treated as proof of arbitrary
scale until their streaming/cursor contracts land.

## Current state

- Dry-run repo candidates are gathered with
  `list_repo_gc_candidates_with_concurrency` into a `Vec<ObjectMeta>`; the
  destructive path uses `plan_repo_gc_candidates_streaming` and durable key
  marks (`crab/src/cmd/gc/mod.rs:500-630`). Its root walk streams history,
  workflow roots, and pack records, while `read_repository_snapshot` still
  materializes the current ref-journal projection.
- `run_gc` computes and grace-filters complete collections before calling
  `execute_deletes`; only the delete chunks are bounded
  (`crab/src/cmd/gc/mod.rs:563-688`, `:707-776`).
- `StoreObjectDeleter::reconcile_manifest` remains a no-op, but durable batch
  outcomes are sufficient for store-only deletion and the deleter declares
  that no process-wide reconciliation key list is required
  (`crab/src/cmd/gc/mod.rs:443-525`).
- Bucket GC materializes all shard/xorb listing results, all referenced shard
  bodies, and all file hashes before deleting (`crab/src/cmd/gc/bucket.rs:169-327`,
  `:636-842`). `delete_or_report` returns only after an in-memory batch
  (`crab/src/cmd/gc/bucket.rs:893-935`).
- Orphaned segmented metadata has a separate direct-delete loop
  (`crab/src/cmd/gc/mod.rs:1315-1370`) and must be routed through the same
  engine. Workflow reachability already exists and remains a root; the engine
  must not invent PB roots.

The durable run is an object-store contract. Use `StoreLayout` path helpers
and conditional puts/CAS; do not make a local directory or SlateDB database the
only recovery source.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| GC unit tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cmd::gc` | all GC tests pass |
| Metadata tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata --locked manifest_store` | all manifest/history tests pass |
| Workflow roots | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-workflow --locked gc` | workflow reachability tests pass |
| Engine tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked gc_run` | new run/journal tests pass |
| CLI contract | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked gc_cli` | plan/execute/resume parser tests pass |
| Format | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- `crates/crab-storage/src/layout.rs` (run/journal path helpers)
- `crab/src/cmd/gc/mod.rs`, `crab/src/cmd/gc/bucket.rs`,
  `crab/src/cmd/gc/parallel_enum.rs`, and focused tests
- a current-layout GC run/journal module under `crab/src/cmd/gc/` or
  `crates/crab-metadata/src/` (choose the layer that owns serialization; keep
  storage I/O behind existing `Store`/metadata contracts)
- `crab/src/main.rs`, `crab/src/core/config.rs`, JSON/JSONL output schemas, and
  current GC docs
- workflow reachability callers only where a streaming/cursor API is required

**Out of scope**:

- writer-fence protocol (Plan 001 owns its semantics; this plan calls it)
- closure sidecars (Plan 003 supplies the strict closure source; destructive
  bucket GC must fail closed when closure coverage is absent)
- provider report parsing (Plan 004 supplies an optional candidate source)
- PB layout, recipe trees, partitioned metadata, or cache-service architecture
- any local-only cache cleanup; `crab prune` remains separate

## Run contract

Define a versioned `GcRun` with:

- `run_id`, scope (`repo` + prefix or bucket), schema version, creator, and
  start/finish timestamps;
- source identity (live-list cursor/profile or inventory identity), grace/force
  policy, and configured memory/delete/list budgets;
- root snapshot: manifest ETag/generation and historical-root digest for each
  repo, ref-registry ETag/generation for bucket scope, workflow/artifact root
  digest, writer-fence epoch, coordinator epoch/protected digest, and closure
  coverage identity when available;
- phase (`planned`, `sealed`, `executing`, `paused`, `complete`, `failed`),
  next batch sequence, and counters;
- immutable batch records containing bounded candidate rows (key, category,
  size, last-modified, content/version identity, and mark proof); and
- append-only or CAS-updated per-candidate outcomes (`pending`, `deleted`,
  `already_absent`, `retained`, `failed`, `replanned`) with error class and
  attempt count.

Use separate current-layout prefixes such as
`{repo}/gc/runs/{run_id}/...` and `.crab/gc/runs/{run_id}/...`, generated by
`StoreLayout`; choose exact names once and test them. A run is executable only
after its roots and all candidate batches are sealed. A partial LIST, malformed
root, incomplete registry, missing closure, stale inventory, or failed plan CAS
must leave the run non-executable.

## Steps

### Step 1: Add the durable run schema and CAS lifecycle

Create strongly typed serializable run, batch, outcome, and root-snapshot
records. Add path helpers and conditional create/CAS methods. Make writes
idempotent by key and sequence; duplicate outcome writes must return the same
record, while conflicting content is a corruption error. Keep schema version
explicit and reject unknown future versions rather than probing alternate keys.

Add `crab gc --resume <run-id>` (or an equivalent single explicit resume form)
and a machine-readable run identifier in dry-run/destructive output. Preserve
existing `--dry-run` as read-only; if the product chooses to persist a preview,
mark it `planned` and never let it execute without an explicit sealed run.
Document whether a plain destructive invocation creates a new run or resumes a
named incomplete run; do not silently choose a run from another repo/bucket.

**Verify**: in-memory object-store tests cover create, duplicate create,
out-of-order batch rejection, CAS conflict, schema mismatch, terminal outcome
idempotence, and scope/run-id validation. `cargo test -p crab --lib --locked
gc_run_schema` passes.

### Step 2: Stream mark and candidate sources under hard budgets

Replace complete collection returns with bounded streams/cursors:

- Enumerate repo prefixes through `object_store` streams and emit fixed-size
  candidate batches directly to the durable run; do not call `try_collect` for
  the full namespace.
- Refactor manifest/history traversal so current roots, ref-journal objects,
  workflow/artifact roots, and historical roots feed a bounded mark writer.
  If a sorted external join is needed, write immutable mark fragments under the
  run prefix and merge them with a bounded heap. Local scratch may accelerate
  the merge but is disposable and never the recovery source.
- Bucket global listing uses the existing adaptive/cost/latency profiles and
  populated two-hex partitions, but emits batches as partitions complete. It
  shares one semaphore for LIST, history reads, closure/inventory reads, and
  metadata work; every buffer and queue has a named byte/row limit.
- Candidate identity is the full object key plus provider version/ETag when
  available. A missing size, malformed hash path, or failed page is a plan
  error, never a skipped row.

Expose peak in-memory rows/bytes, scratch bytes, open files, list calls, and
delete calls in the run outcome. Keep `compute_unreachable` as a pure small-set
helper for unit tests, but route production GC through the bounded path.

**Verify**: a synthetic namespace at least 10× the configured memory budget
completes with a bounded high-water metric; a faulted LIST produces no sealed
executable run; cancellation closes all streams and leaves a resumable run.
Run `cargo test -p crab --lib --locked gc_streaming`.

### Step 3: Separate plan, seal, execute, and resume

Refactor repo and bucket entry points to share one engine:

1. `plan`: capture roots and writer/coordinator proof, stream marks/candidates,
   apply grace/force policy, and seal immutable batches.
2. `execute`: for each batch, acquire Plan 001's exclusive domain fence,
   reload and compare every root/epoch/coverage identity, re-mark the bounded
   batch, and transition only unchanged candidates to `pending`.
3. `delete`: issue bounded idempotent deletes, record each outcome before
   releasing the fence, and treat provider NotFound as `already_absent`.
4. `reconcile`: update current manifest/ref-registry/file-index metadata only
   through their existing CAS contracts; record reconciliation failure without
   losing per-object outcomes.
5. `resume`: load the sealed run, skip terminal outcomes, revalidate roots,
   and retry only pending/retryable failures. A root drift creates a new batch
   or marks the old batch `replanned`; it never reuses stale candidates.

Route `cleanup_orphaned_bulk_objects`, bucket shard/xorb deletes, and file-index
tombstones through this engine. Keep dry-run side-effect-free except for
explicitly documented output; it must not take a destructive fence.

**Verify**: barrier tests kill the process after plan seal, after each delete,
after outcome write, during reconciliation, and during fence release. A resume
completes exactly once per candidate and never deletes a newly reachable object.
Run `cargo test -p crab --lib --locked gc_resume` and
`cargo test -p crab --lib --locked cmd::gc`.

### Step 4: Make retries and provider failures explicit

Classify errors using the existing `CrabError`/provider mapping. Retry transient
GET/LIST/DELETE/CAS failures with bounded backoff and attempt count; pause a run
when the retry budget is exhausted. Do not treat parse errors, missing roots,
partial listings, version conflicts, or lease loss as transient. Persist the
terminal reason and expose it in text/JSON/JSONL output.

Use conditional delete/version checks where the provider supports them. If a
provider cannot pin a version, require the root revalidation plus object age and
report that limitation in the run proof; never claim a stronger guarantee than
the provider contract. Preserve successful deletes when a sibling fails.

**Verify**: fault-injection tests cover timeout, throttling, NotFound, stale
ETag, partial page, corrupted batch, cancellation, and lease renewal failure;
the run remains resumable and emits one terminal outcome per key.

### Step 5: Expose and document operational controls

Add only controls that existing config cannot express: explicit resume/run ID,
bounded memory or batch byte limit if required by the engine, and a source
selector for live list versus Plan 004 inventory. Resolve grace from config
when omitted, as Plan 001 specifies. Include run ID, phase, root proof,
candidate counts, counters, retry state, and budget high-water marks in
structured output without credentials.

Document operator recovery for paused/failed runs, process death, provider
outage, root drift, and a corrupt journal. Make “delete the journal” an
unsupported action; add an explicit administrative quarantine/repair command
only if code proves it is safe.

**Verify**: CLI/help/config/doc tests show one plan/execute/resume contract;
JSON and JSONL schemas round-trip a paused, resumed, partial, and complete run.

## Test plan

- Schema/CAS property tests and object-store idempotence tests.
- Stream/back-pressure tests with large synthetic namespaces and bounded
  memory accounting.
- Root/fence drift tests for repo, bucket, history, workflow, registry, and
  coordinator domains.
- Process death and cancellation at every persisted transition.
- Provider fault tests for pagination, throttling, conditional delete, missing
  object, and stale version.
- End-to-end in-memory repo and bucket runs proving current/history/workflow
  roots survive and successful deletes resume safely.

Model tests after existing `run_gc`, `execute_deletes`, bucket listing, CAS,
and workflow GC tests; retain the current pure helpers as small unit tests but
do not use them as production-scale proof.

## Done criteria

- [x] Destructive production GC no longer collects a complete candidate
      namespace, and repo roots/pack records flow through durable marks. Dry
      runs and bounded snapshot/artifact joins remain explicit follow-up work.
- [ ] Every destructive run has a durable sealed plan, root proof, batch records,
      and per-object terminal outcomes.
- [ ] Process death/cancellation/provider failure resumes without duplicate
      deletes or lost sibling outcomes.
- [ ] Repo, bucket, orphaned bulk, and file-index cleanup use one engine and
      Plan 001's fence; no direct delete bypass remains.
- [ ] Memory, scratch, open-file, and request budgets are measured and enforced
      for every remaining snapshot/artifact/closure join.
- [ ] CLI, JSON/JSONL, docs, focused tests, and full `cmd::gc` tests pass.
- [ ] No PB-only roots or local filesystem state is required for correctness.

## STOP conditions

- A required reachability source cannot be streamed or persisted as a bounded
  mark fragment without changing the current manifest contract.
- A provider cannot provide enough object identity to safely distinguish a
  candidate from a replacement object.
- A process-death test needs local scratch to recover because the run journal is
  incomplete.
- Root/fence drift would be handled by silently deleting the old batch.
- A direct delete remains outside the shared engine or a test needs to weaken a
  current reachability invariant.
- Any verification command fails twice after a reasonable fix attempt.

## Maintenance notes

The run schema, paths, outcome meanings, and root-proof fields are persistent
contracts. Any new GC-managed namespace must supply a streaming candidate
adapter and a reachability root before it is added to the engine. Plan 003's
closure source and Plan 004's inventory source must implement the same bounded
candidate interface. Reviewers should inspect memory accounting, CAS ordering,
idempotent outcome writes, and the exact point at which the fence is released.
