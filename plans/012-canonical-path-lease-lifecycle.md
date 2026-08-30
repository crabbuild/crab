# Plan 012: Give each staged path one canonical lease lifecycle

> **Executor instructions**: Execute only after Plan 011 lands. Follow each
> verification gate and stop rather than deleting an ownership record whose
> push-snapshot relationship is unclear. Update the status row in
> `plans/README.md` when complete unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 86139f55..HEAD -- crates/crab-staging/src/index.rs crates/crab-staging/src/lib.rs crates/crab-staging/src/stats.rs crates/crab-staging/src/stream.rs crab/src/cmd/add.rs crab/src/cmd/doctor.rs crab/src/cmd/staging.rs crab/src/git/clean.rs crab/src/git/worktree.rs crab/src/import/ingest.rs crab/src/lfs/migrate.rs`
> Plan 011 is expected to change several paths. Rebase this plan onto its final
> publication-intent and promotion APIs before editing; if those APIs do not
> exist, stop.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/011-transactional-add-publication.md`
- **Category**: perf, tech-debt
- **Planned at**: commit `86139f55`, 2026-08-29
- **Delivery status**: DONE — functional, broad, scale-proxy, schema, and
  RustFS acceptance proof passes in the combined v1 hardening change set.

## Why this matters

Every add attempt can currently retain another lease for the same path. Push
retires only the exact leases captured by its committed snapshot, so repeated
edit/add-before-push cycles and abandoned skip batches retain obsolete recipes,
segments, prepared xorbs, and database rows indefinitely. The fix needs one
canonical current owner per path while preserving immutable pins for concurrent
push snapshots. Versions already committed but not yet pushed also need
content-addressed history owners; they are durable Git dependencies, not
discardable edit history.

## Current state

- `crates/crab-staging/src/index.rs:486-495` keys path leases by
  `(batch_id, path_bytes)`, allowing unlimited versions of one path.
- `crates/crab-staging/src/index.rs:2868-2877` upserts only inside the current
  batch; it never supersedes prior batches for the same path.
- `crates/crab-staging/src/index.rs:2318-2334` copies exact leases into an open
  push snapshot. These immutable pins must survive a concurrent restage.
- `crates/crab-staging/src/index.rs:2383-2525` retires only captured committed
  snapshot leases and avoids deleting leases pinned by another open snapshot.
- `crates/crab-staging/src/lib.rs:2587-2614` cleans push snapshots and orphan
  segments but does not reclaim unrelated stale path leases.
- `crates/crab-staging/src/stats.rs:28-35` already exposes lifecycle counts,
  including `path_leases`; extend this surface rather than inventing another
  diagnostics format.
- The lifecycle test `crates/crab-staging/src/stream.rs:1644` proves an open
  batch can retain the same recipe after committed retirement. Preserve the
  concurrency invariant, but make the owner eventually reclaimable.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Staging lifecycle | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked` | all pass |
| Doctor tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cmd::doctor::` | all matching tests pass |
| Staging CLI tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cmd::staging::` | all matching tests pass |

## Scope

**In scope**:

- `crates/crab-staging/src/index.rs`
- `crates/crab-staging/src/lib.rs`
- `crates/crab-staging/src/stats.rs`
- `crates/crab-staging/src/stream.rs` for lifecycle tests
- `crab/src/cmd/add.rs` only at Plan 011's publication/promote call sites
- `crab/src/git/clean.rs` and `crab/src/git/worktree.rs` for preserving the
  prior committed recipe when ordinary `git add` replaces a path head
- `crab/src/import/ingest.rs` and `crab/src/lfs/migrate.rs` at history-building
  publication call sites that emit several committed versions before push
- `crab/src/cmd/doctor.rs`
- `crab/src/cmd/staging.rs` if this command owns staging diagnostics

**Out of scope**:

- Deleting any lease pinned by an open push snapshot.
- Age-based automatic deletion of current staged data.
- Remote object GC, manifest retirement, pointer format, or xorb packing.
- Reintroducing complete-file scanning to decide which lease is current.
- New user-configurable retention knobs.

## Git workflow

- Branch: `advisor/012-canonical-path-leases`
- Commit by logical unit: schema/transaction, reclamation, diagnostics/tests.
- Example messages: `refactor: make staged path ownership canonical`,
  `test: cover concurrent restage lease retirement`.
- Read `crates/AGENTS.md` before shared-crate edits.

## Steps

### Step 1: Characterize path ownership and concurrent snapshots

Add table-driven tests for:

- add path content A → add content B before push: B is current; A becomes reclaimable;
- add content A → open push snapshot → add content B: A remains pinned until the snapshot
  commits/retires or aborts;
- two paths sharing one recipe: superseding one path does not reclaim bytes
  still owned by the other;
- direct prepared and segment authority follow identical lease rules;
- skip/open lease promoted by Plan 011 becomes superseded and reclaimable;
- repeated identical adds keep one current owner instead of growing leases.

Use the exact-snapshot tests near `crates/crab-staging/src/index.rs:4978` and
the shared-recipe test at `crates/crab-staging/src/stream.rs:1644` as patterns.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked path_owner`
→ all new ownership tests pass after Steps 2-3.

### Step 2: Separate current path ownership from immutable snapshot pins

Add one canonical path-head table keyed by exact `path_bytes`. Its row must
identify the current batch, file hash, and recipe hash. Update the Plan 011
publication/promote transaction so replacing a path head and recording the new
published owner happen atomically.

Keep immutable push snapshot rows as historical pins. Do not overload a
`current` boolean on every old lease: one unique head row is easier to enforce
and query. This is a hard cutover of canonical staging schema v1: create path
heads for all new writes and delete the prior headless reader/writer path. Do
not backfill old local databases or keep a “latest timestamp” fallback.
Pre-cutover staging state must be rejected with the same remove/restage
instruction defined by Plan 011. Open skip leases are never heads until
promoted or directly published.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked path_head`
→ fresh-schema, uniqueness, atomic replacement, and retired-schema rejection
tests pass.

### Step 3: Reclaim superseded ownership after pins release

When a publication transaction replaces a path head:

1. Mark the old lease superseded or remove its ordinary path ownership.
2. Preserve its recipe/payload while any open snapshot pins it or another path
   head/lease references it.
3. Reclaim its segment rows and prepared payload files only after the final
   owner and snapshot pin disappear, using existing `retire_file_if_unleased`
   and file unregister mechanics.
4. After snapshot commit/retirement or open-snapshot cleanup, run the same
   bounded reclaim query so formerly pinned superseded owners do not leak.

Keep all SQLite ownership changes transactional. Filesystem payload deletion
may follow commit through the existing idempotent retirement routines; a crash
between metadata and file deletion must be recoverable by `sweep_orphans`.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked superseded`
→ segment/prepared byte counts return to the expected current, committed
history-owner, and snapshot set.

### Step 4: Expose actionable lifecycle health

Extend `StagingLifecycleHealth` and doctor/staging output with at least:

- open batches with no publication intent;
- unresolved Plan 011 publication intents;
- current path heads;
- superseded leases still pinned by open snapshots;
- reclaimable superseded leases;
- prepared payload bytes and segment bytes retained by each class.

Doctor must distinguish “safe but pinned” from “ambiguous publication” and
“reclaimable leak.” Human output can summarize; JSON fields must be stable and
typed if the command already exposes a schema. Do not silently clean ambiguous
state from doctor.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked staging_lifecycle`
→ diagnostic tests assert each state and remediation.

## Test plan

- Unit: path-head uniqueness, atomic replacement, retired-schema rejection,
  shared-recipe reference counts, direct/segment parity.
- Concurrency: open snapshot on content A while content B becomes current; retiring either
  order never removes needed authority.
- Crash recovery: SQLite replacement committed before payload cleanup; reopen
  and orphan sweep converge without data loss.
- Scale proxy: restage one path 1,000 times with small fixture chunks; after
  cleanup, lease/recipe/payload counts are bounded by current heads,
  committed-but-unpushed history owners, and open snapshot pins, not
  uncommitted edit history.

## Done criteria

- [x] Every publishable path has at most one canonical current head.
- [x] Concurrent push snapshots retain exact historical authority until release.
- [x] Repeated uncommitted restaging does not grow retained payload/lease counts
  with edit history; committed, unpushed versions remain explicitly owned.
- [x] Direct `crab add` and clean-filter publication preserve earlier committed
  versions reachable from `HEAD` or local ref tips through first push,
  including skip-add preparation promoted by ordinary `git add` and versions
  retained on another branch.
- [x] Open skip batches are visible in diagnostics and reclaimed after promotion/rollback.
- [x] Direct prepared and segment authorities pass the same lifecycle matrix.
- [x] Focused staging, CLI diagnostics, format, schema, broad library, scale
  proxy, and RustFS checks pass.
- [x] The combined v1 hardening change set is committed locally; canonical
  path heads and immutable push pins share one lifecycle owner.

## STOP conditions

Stop and report if:

- Plan 011 does not provide an atomic publication/promote transaction.
- Plan 011 retained any pre-cutover staging-schema reader or backfill path.
- Snapshot readers dereference ordinary path leases after capture rather than
  their immutable snapshot rows; deleting an old owner would then be unsafe.
- Reclamation requires scanning or hashing every staged payload on each add.
- A change would make open recipes push-visible.

## Maintenance notes

Path heads answer “what would a new push snapshot capture now”; snapshot pins
answer “what must remain for an already-started push.” Keep those concepts
separate in future schema changes. Reviewers should demand before/after counts
for leases, recipes, segment bytes, and prepared bytes under concurrent restage.
Long-horizon recipe paging remains in `plans/004-bounded-local-recipes.md` and
is not part of this lifecycle change.
