# Plan 015: Bound and compose prepared-xorb push work per file and source xorb

> **Executor instructions**: Preserve every existing integrity check while
> removing push-wide fallback and repeated full-source reads. Land the work in
> measured steps; do not combine it with add-time remote classification. Update
> `plans/README.md` when complete unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 86139f55..HEAD -- crab/src/git/push.rs crates/crab-staging/src/lib.rs crates/crab-staging/src/stats.rs crates/crab-storage/src/store.rs crates/crab-xet/src/xorb/format.rs`
> If prepared-plan adoption, residual reads, upload payloads, or staged
> multipart APIs changed, reconcile every current-state excerpt before editing.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/011-transactional-add-publication.md`
- **Category**: perf, tests
- **Planned at**: commit `86139f55`, 2026-08-29
- **Delivery status**: DONE — counter-based, focused, full-workspace, E2E, and
  RustFS acceptance proof passes in the combined v1 hardening change set.

## Why this matters

Prepared xorbs avoid segment rewrites, but current push consumption loses much
of that advantage in mixed workloads. One file without a valid plan disables
plan adoption for every file; sparse residual chunks can reread and hash the
same complete prepared xorb once per 16 MiB pack batch; and protected pushes
materialize file-backed xorbs despite an existing staged multipart-file API.
This plan makes authority decisions per file, reads each source xorb once under
an explicit bound, and streams file-backed protected uploads.

## Current state

- `crab/src/git/push.rs:10532-10623` models add-time adoption as one push-wide
  boolean; one missing/stale/mismatched plan returns `false` for all files.
- `crab/src/git/push.rs:10790-10799` correctly treats one prepared xorb as an
  all-or-nothing upload when any chunk already has verified remote placement.
  Preserve this rule and residual coverage.
- `crab/src/git/push.rs:11297-11340` batches staging reads by configured payload
  bytes, commonly 16 MiB.
- `crates/crab-staging/src/lib.rs:1232-1308` groups prepared requests only
  within one batch, then spawns one task per represented xorb. Each task reads,
  hashes, parses, and digest-verifies the complete xorb before decoding wanted
  chunks. The same source xorb can repeat across batches, and task concurrency
  is not separately bounded.
- `crab/src/git/push.rs:11724-11770` permits file-backed multipart only for a
  non-staging store; protected/staged writes call `read_bytes` instead.
- `crates/crab-storage/src/store.rs:1401-1446` already supports retrying a
  multipart upload from a local file and records staged canonical→write-path
  mappings. Test `:2221` proves the storage-level contract.
- Existing push test `crab/src/git/push.rs:28326` covers complete prepared-plan
  adoption; `:28995` covers a residual backed by segment authority rather than
  prepared-only authority; `:30204` covers file streaming only for a normal
  store.
- The current local prepared push-plan version is not a compatibility contract.
  This work leaves one canonical plan format named v1 and may invalidate all
  pre-cutover local plans; restage instead of adding a legacy reader.
- Do not duplicate `plans/004-bounded-local-recipes.md` (paged recipe metadata)
  or `plans/006-streaming-dedup-and-push.md` (classification during add).

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Staging tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked` | all pass |
| Storage tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-storage --locked staging_multipart_file` | all matching tests pass |
| Push tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked add_time_push_plan -- --nocapture` | all matching tests pass |

## Scope

**In scope**:

- `crab/src/git/push.rs`
- `crates/crab-staging/src/lib.rs`
- `crates/crab-staging/src/stats.rs` for byte/open/concurrency metrics
- `crates/crab-storage/src/store.rs`
- adjacent tests in those modules

**Out of scope**:

- Changing xorb format, compression, target size, or immutable identity.
- Add-time remote lookup/classification or per-file direct-staging selection.
- Paged recipe storage.
- Weakening full payload hash, footer, placement, or chunk verification.
- A new user-facing memory/concurrency config unless existing push limits
  cannot express the bound; stop first rather than adding one.
- Shard partitioning and CAS-replan metadata (Plans 013-014).
- Push-plan v2+, compatibility readers, dual writes, or migration of old local
  plan files.

## Git workflow

- Branch: `advisor/015-bound-prepared-push`
- Commit by measured behavior: tests/metrics, per-file adoption, xorb-aware
  residuals, protected streaming.
- Example message: `perf: bound prepared xorb residual reads`.
- Read `crates/AGENTS.md` before shared-crate edits.

## Steps

### Step 1: Add byte/open/concurrency characterization

Extend test metrics without changing production output to count:

- prepared source-xorb full-file opens and bytes read;
- maximum concurrent prepared source readers;
- decoded residual bytes;
- file-backed versus materialized upload bytes;
- per-file plan adoption/fallback outcomes.

Add adversarial tests:

1. One prepared-only source xorb larger than the read batch with sparse
   residual chunks spanning several batches.
2. One residual batch referencing multiple maximum-sized source xorbs.
3. A push with file A's valid prepared plan and file B missing/stale plan.
4. A file-backed prepared xorb through a protected staging store.

Assertions must use counters, not wall-clock thresholds. Pre-fix tests should
demonstrate repeated source bytes and push-wide fallback, then pass after the
relevant steps.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked prepared_authority_metrics -- --nocapture`
→ deterministic counter assertions pass after Steps 2-4.

### Step 2: Replace the push-wide plan bit with per-file authority results

Refactor `try_apply_add_push_plans` to validate and adopt each pointer file
independently. Return a narrow result keyed by file hash describing:

- verified remote placements;
- adopted prepared xorbs;
- residual chunk occurrences that need local decoding/packing;
- normal-classification fallback for only that file;
- the reason/metric for fallback.

Merge results with explicit conflict checks for shared chunks/xorbs. One stale
plan must not discard already-validated siblings. A corrupt plan may fall back
only when the same verified recipe has another complete local authority;
otherwise fail rather than hiding corruption. Delete the global
`add_push_plan_applied` mode once all consumers use per-file/per-chunk coverage.

Hard-cut the local serialized push-plan contract to the final v1 shape needed
by this result. Delete older version enums/readers/tests. Encountering a
pre-cutover plan must produce one actionable restage error; it must not
silently fall back or translate the plan.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked mixed_add_time_push_plan -- --nocapture`
→ valid prepared files are adopted, only uncovered files classify normally,
and shared-placement conflicts fail.

### Step 3: Make residual iteration source-xorb aware and explicitly bounded

Change the prepared-only residual path so all wanted occurrences for one source
xorb are coalesced before decoding. Each source payload must be opened, fully
hashed, parsed, and digest-verified at most once per push attempt. Decode every
wanted chunk from that verified parser, then release the complete source bytes
before opening more than the explicit concurrency bound.

Use a semaphore or bounded worker set derived from existing push payload/read
limits. The bound must cover complete source-xorb bytes, not only decoded
residual bytes. Preserve output occurrence order, repeated-hash semantics,
cancellation, and corruption errors. If the parser cannot operate without one
complete payload, keep one bounded complete payload; do not build an unverified
range reader in this plan.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked prepared_residual`
→ each source opens once, bytes read equal the unique source-xorb bytes, max
concurrency is within the asserted bound, and corrupt payload/chunk cases fail.

### Step 4: Stream file-backed xorbs through protected staged writes

Route file-backed payloads through `Store::put_multipart_file_retry` regardless
of whether the store uses a staging write prefix. Preserve:

- canonical object hash verification;
- staged canonical→write-path recording;
- retry/abort behavior;
- payload permits and progress accounting;
- service-owned finalization/receipt identity.

Keep the in-memory PUT path only for genuinely in-memory payloads. Do not read
the file once merely to choose the upload API. Add a push-composition test, not
only the existing storage unit test, proving protected file-backed upload has
zero full-body materialization and records the staged mapping.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked protected_file_backed_xorb -- --nocapture`
→ upload succeeds, staged mapping is exact, and materialized-byte counter is 0.

### Step 5: Preserve integrity and retry behavior across the composed path

Run the prepared plan corruption, stale remote ref, upload resume, sibling
failure, and cancellation tests. Add one end-to-end matrix combining mixed
plan adoption, prepared-only residuals, protected streaming, shard build, and
byte-identical hydrate.

Do not defer payload verification until after any remote object or metadata is
admitted. Persisting successful upload proof earlier is valuable follow-up,
but it requires an origin identity/receipt design and is outside this plan.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --test e2e_add_commit_push --locked prepared -- --nocapture`
→ the prepared E2E matrix passes with exact byte reconstruction.

## Test plan

- Unit: per-file plan validation and conflict merging; source-xorb coalescing;
  concurrency bound; corruption and cancellation; staged multipart mapping.
- Integration: mixed valid/no-plan files; verified overlap producing sparse
  prepared-only residuals; protected file upload; retry without reupload;
  byte-identical hydrate.
- Performance counters: source bytes are O(unique prepared source bytes), not
  O(pack batches × source bytes); protected materialized bytes remain zero for
  file-backed payloads.

## Done criteria

- [x] One missing/stale plan no longer disables valid sibling plans.
- [x] Every prepared source xorb is fully read/verified at most once per push attempt.
- [x] Prepared-source reader concurrency has an explicit byte-aware bound.
- [x] Protected file-backed uploads use staged multipart without full-body materialization.
- [x] All existing payload/hash/footer/placement integrity checks remain enforced.
- [x] Prepared push plans have one canonical v1 serializer/reader; old local plans require restaging.
- [x] Counter-based adversarial tests and byte-identical E2E pass.
- [x] Focused staging, storage, push, and format commands pass.
- [x] The combined v1 hardening change set is committed locally; per-file plan
  adoption and prepared-source bounds use one canonical push path.

## STOP conditions

Stop and report if:

- Per-file adoption cannot resolve shared chunk/xorb ownership without a new
  durable plan format.
- Bounding complete source bytes requires changing the xorb parser or format.
- Protected multipart completion cannot return/bind the same canonical object
  identity used by current service finalization.
- Any optimization would skip a full payload digest or exact chunk verification.
- The work begins implementing add-time classification or recipe paging already
  owned by Plans 006 and 004.

## Maintenance notes

Push memory limits must account for source authority bytes, decoded residuals,
packed output, and upload buffers—not only one of those pools. Reviewers should
require counter evidence for each. If future work adds verified range parsing,
it may reduce the one-complete-source bound, but it must keep the same payload
digest and chunk-placement guarantees.
