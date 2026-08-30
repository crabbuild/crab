# Plan 016: Canonical prepared chunk authority

> **Executor instructions**: Replace per-file prepared ownership with one
> preparation-wide, normalized authority model. Preserve Plans 011, 012, and
> 015's publication, lease, snapshot, integrity, and bounded-I/O contracts.
> This is a hard v1 cutover: delete obsolete readers, sidecars, tables, and
> tests; do not add migration, compatibility, feature flags, or v2 names.
>
> **Drift check (run first)**:
> `git diff --stat d7d0ee8e..HEAD -- crab/src/cmd/add.rs crab/src/git/push.rs crates/crab-staging/src/{add_push_plan,index,lib,push_plan,stats,stream}.rs crab/tests/e2e_add_commit_push.rs crab/docs/design/{add,push}.md`
> If direct-stream builder ownership, prepared-plan persistence, publication
> intents, path heads, push snapshots, or prepared adoption changed, rebuild
> the evidence map below before editing. Read `crates/AGENTS.md` before shared-
> crate changes.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/011-transactional-add-publication.md`,
  `plans/012-canonical-path-lease-lifecycle.md`,
  `plans/015-bound-prepared-xorb-push.md`
- **Category**: correctness, durability, performance, tests
- **Planned at**: commit `d7d0ee8e`, 2026-08-29
- **Delivery status**: DELIVERED

## Outcome

After this plan, `crab add` assigns every staged recipe chunk one deterministic
authority:

1. a validated committed remote placement;
2. one sealed local prepared-xorb placement;
3. one verified staging segment; or
4. for bytes not represented by 1-3, one preparation-wide claim that produces
   exactly one sealed local prepared placement.

Multiple files, repeated chunks, later `crab add` invocations, and restaging
may reference that authority, but they cannot independently compress the same
chunk into conflicting xorbs. `push` reads normalized recipe authority, opens
and validates each unique local payload once, uploads each missing xorb once,
builds dependency-closed shards, and publishes refs only after all immutable
data is durable. A direct prepared chunk does not require a redundant segment
copy.

The local prepared payload is stored content-addressably, relative to the
repository staging root:

```text
.crab/staging/push-plans/payloads/<first-two>/<xorb-hash>.xorb
```

This is local pending authority, not a bucket object. On push, its immutable
remote body is written to:

```text
.crab/xorbs/<first-two>/<xorb-hash>
```

The bucket-global SlateDB at `.crab/chunk_index_db/` continues to contain only
committed, origin-bound remote placement receipts. It is read during add to
avoid local work and updated only after manifest/ref CAS succeeds. It never
stores a provisional add claim or prepared payload.

## Why this matters

Direct multi-file streaming currently creates an independent `XorbBuilder` per
file. Two partially overlapping files can therefore place the same new chunk
in different prepared xorbs. The later push merge correctly detects the
disagreement and fails before uploading or advancing refs, but the user can
successfully add and commit a state that cannot be pushed.

Disabling multi-file direct preparation would restore correctness but discard
the intended one-pass, no-segment-copy path. Choosing one candidate at push
would hide a producer invariant violation and could upload redundant bytes.
The correct ownership boundary is the add preparation spanning all files:
claim each new chunk once before a per-file builder receives it, then link all
recipe occurrences to the one sealed result.

## Evidence map and current state

| Surface | Current evidence | Consequence |
|---|---|---|
| Entry point | `crab/src/cmd/add.rs:1875-1897` creates a shared factory that returns per-file builders; `:2013-2027` permits direct streaming for multi-file groups. | Builder capacity is shared, but chunk ownership is not. |
| Streaming owner | `crates/crab-staging/src/stream.rs:65` calls the abstraction a factory for per-file builders; `:1200-1238` persists one prepared plan per file. | Partially overlapping files can produce two placements. |
| Existing canonical sibling | `crates/crab-staging/src/add_push_plan.rs:303-326` uses one builder and a global `queued_chunks`; `:491-553` links one xorb to sibling file plans. | The non-direct planner already proves the required uniqueness model. |
| Durable local schema | `crates/crab-staging/src/index.rs:683-750` keys prepared xorbs and their chunks by `file_hash`; `:890-905` separately has normalized `prepared_payloads` and `prepared_leases`. | Payload lifetime is partly normalized while placement authority remains duplicated per file. |
| Cross-add lookup | `crates/crab-staging/src/index.rs:6087-6171` orders candidate placements by file/xorb and selects the first. | Multiple local placements are representable; lookup resolves ambiguity rather than forbidding it. |
| Recipe sealing | `crates/crab-staging/src/index.rs:4171-4200` derives leases through per-file prepared tables. | Recipe lifetime is coupled to obsolete per-file ownership. |
| Push guard | `crab/src/git/push.rs:11183-11264` rejects prepared xorbs that disagree on a chunk placement. | Failure is safe but too late; committed Git state can be unpushable. |
| Push ordering | `crab/src/git/push.rs:16485-16508` classifies authority before immutable uploads and ref publication. | Current disagreement does not corrupt the remote. Preserve this ordering. |
| Staging concurrency | `crates/crab-staging/src/lib.rs:19-27` gives writers an exclusive staging lock while push uses a shared lock. | Separate add processes serialize; workers inside one add need transaction-wide coordination. |
| Remote proof | `crab/src/git/push.rs:15117-15193` validates bucket-global chunk receipts during add; `:15303-15318` commits the rebuildable index after manifest CAS. | SlateDB is a committed-remote accelerator, not a pending ownership database. |
| Local/remote layout | `crates/crab-staging/src/push_plan.rs:454-458` stores local payloads per file; `crates/crab-storage/src/layout.rs:139-159` defines bucket-global sharded xorb keys. | Hard-cut local storage to one content-addressed payload without changing remote xorb identity. |
| Existing tests | `crab/src/git/push.rs:29591-29757` covers cross-file linking through the non-direct planner. | CI does not exercise partial overlap through the actual direct-stream CLI path. |

Important current behavior: when direct xorb preparation is active,
`stream.rs` does not also stage segment payload for chunks fed to the builder.
The prepared payload can be the only local byte authority. Recovery and final
publication therefore must prove complete recipe coverage; push cannot assume
a segment fallback exists.

## Design

### 1. Deterministic authority selection

Resolve chunk hashes in bounded batches and in this order:

1. **Committed remote**: accept only the existing origin-, generation-, shard-,
   and placement-validated receipt. Record recipe remote authority and produce
   no local payload.
2. **Sealed local prepared**: if one normalized, digest-verified placement
   already exists, attach a recipe lease. Do not recompress or copy it.
3. **Verified staging segment**: reuse existing segment authority. Do not
   repack it during add merely to make a prepared xorb.
4. **New bytes**: atomically claim the chunk in the current add preparation.
   Only the claim owner feeds its per-file builder. Every other occurrence
   records a dependency on the owner result.

Remote wins over local because it eliminates upload and local retention. A
sealed prepared payload wins over a segment because it is already uploadable.
A segment wins over new preparation because recompression and a second local
body have no payoff. Only an actual committed receipt qualifies as remote;
HEAD/cache guesses and uncommitted uploads do not.

Authority is selected per recipe occurrence, but physical payload ownership is
global within the staging database. There may be several recipe references to
one chunk placement and several recipe leases on one payload. There must never
be two live prepared placements for the same chunk hash.

### 2. Hard-cut normalized v1 schema

Retain and strengthen the existing normalized payload/lease concepts. Replace
the per-file prepared tables and serialized plan copies with these canonical
relationships (exact column types should follow current schema conventions):

```sql
prepared_payloads(
  xorb_hash PRIMARY KEY,
  payload_size,
  payload_path,
  footer_digest,
  created_at
)

prepared_payload_chunks(
  xorb_hash REFERENCES prepared_payloads ON DELETE CASCADE,
  chunk_index,
  chunk_hash UNIQUE,
  uncompressed_size,
  compressed_offset,
  compressed_size,
  PRIMARY KEY (xorb_hash, chunk_index)
)

prepared_leases(
  recipe_hash,
  xorb_hash REFERENCES prepared_payloads ON DELETE RESTRICT,
  PRIMARY KEY (recipe_hash, xorb_hash)
)

add_preparations(
  preparation_id PRIMARY KEY,
  state CHECK (state IN ('recording', 'sealing')),
  created_at
)

prepared_chunk_claims(
  chunk_hash PRIMARY KEY,
  preparation_id REFERENCES add_preparations ON DELETE CASCADE,
  owner_batch_id,
  owner_ordinal,
  uncompressed_size
)
```

`prepared_payload_chunks.chunk_hash UNIQUE` is the durable invariant that push
currently tries to reconstruct from conflicting per-file plans. Claims are
temporary preparation state and disappear only after successful whole-
preparation finalization or rollback.

Keep the existing normalized recipe remote-authority relationship. If current
segment tables cannot express exact recipe authority without scanning, add the
smallest recipe-to-segment relationship needed; do not create a second generic
plan document. Runtime push DTOs are derived from normalized rows and do not
need a persisted JSON representation.

Delete in the same cutover:

- `prepared_xorbs` and `prepared_xorb_chunks` keyed by `file_hash`;
- persisted `file_push_plans.plan_json` and per-file xorb sidecars;
- per-file local payload paths under `push-plans/xorbs/<file>/<xorb>`;
- legacy serializers/readers, migrations, aliases, fallbacks, and their tests.

The staging schema remains v1. Opening an old development staging database
must fail with one actionable remove/restage instruction. Do not translate it.

### 3. One preparation across all direct-stream files

Create one `add_preparation` before scheduling the direct-stream file batches.
It spans every file eligible for the same add publication, not one builder or
one file. Keep the current bounded worker and builder counts.

For each bounded chunk batch:

1. perform the committed-remote lookup already used by add;
2. query sealed local prepared placements in one batch;
3. query verified staging segments in one batch;
4. insert claims for remaining unique hashes in one SQLite transaction;
5. return claim outcomes to workers;
6. feed bytes only to the winning occurrence's builder;
7. record all recipe occurrences against either existing authority or the
   owning claim.

Use SQLite as disk-backed coordination. Do not retain a repository-sized hash
map in memory. A small batch-local cache is allowed only as an optimization;
it is never authority. The existing exclusive staging writer lock serializes
separate `crab add` processes, while the uniqueness constraint coordinates all
workers in the active process and prevents future call sites from bypassing
the invariant.

When a builder seals an xorb:

1. finish and validate the xorb footer, full payload hash, and every placement;
2. write to a same-filesystem temporary file;
3. flush file contents and required parent metadata;
4. atomically rename it to
   `push-plans/payloads/<first-two>/<xorb-hash>.xorb`;
5. stage its normalized payload/chunk rows for preparation finalization.

If two equivalent xorb bodies converge on the same final hash, the verified
existing body may be reused. A body with the same name but wrong digest is
corruption and fails closed.

### 4. Whole-preparation finalization and Git publication

Finalize all member batches in one SQLite transaction:

- insert every sealed payload and its unique placements;
- resolve non-owner recipe occurrences through their owner's placement;
- attach recipe payload leases and committed remote proofs;
- preserve verified segment relationships;
- prove every recipe occurrence has exactly one selected authority;
- seal recipes and install canonical path heads;
- delete preparation claims and the preparation record.

Only after that transaction succeeds may Plan 011's publication intent replace
the Git index. Do not publish a subset of files whose owner payload happened to
seal first. On any file/worker failure, cancel the preparation, roll back all
member open batches, release claims, and leave the old path heads/Git index
unchanged.

Plan 012 remains the lifetime owner: restaging replaces path heads and releases
unowned leases; an open push snapshot pins the exact recipes and payloads it
observed. Plan 015 remains the I/O owner: push opens each distinct prepared
payload once under the established byte-aware bound.

### 5. Recovery and reclamation

Recovery must converge for each interruption point:

| Interruption | Required recovery |
|---|---|
| Before payload rename | Delete abandoned temp body; roll back preparation and open batches. |
| After rename, before SQLite finalization | Treat final body as unleased; verify and adopt only when the matching preparation can finish, otherwise sweep it. |
| During SQLite finalization | Transaction rollback leaves no partial payload placement or recipe lease. |
| After finalization, before Git index replacement | Existing publication-intent reconciliation completes or retires the exact sealed recipes. |
| After Git index replacement | Existing publication reconciliation confirms the index identity and canonical heads. |
| During push/restage overlap | Push snapshot pins old recipes/payloads until retirement; new heads may become current independently. |

On staging open, unresolved preparations must be rolled back as one unit. Do
not guess which member files were “probably complete.” Orphan sweeping must
respect live recipe leases, open publication intents, and push snapshots.

### 6. Push consumption

Replace per-file JSON plan adoption with a normalized, snapshot-bound authority
query. It must return bounded pages of:

- committed remote placements;
- distinct prepared payloads and their complete placements;
- segment-backed residual occurrences.

Push validates every selected payload once, uploads every missing xorb once,
and builds one placement map per chunk. Keep the current disagreement check as
an impossible-state assertion and corruption diagnostic even though the
schema forbids the state.

If a prepared body is missing or corrupt, push may repack only when the recipe
has independently verified segment authority for every affected occurrence.
Direct-prepared-only authority has no such fallback and must fail closed with
an actionable restage message. Never choose an arbitrary xorb, weaken digest
checks, or silently omit a recipe lease.

Remote receipt publication remains post-manifest/ref CAS. Do not write a
prepared claim, local path, pending upload, or uncommitted placement into the
bucket-global SlateDB.

### 7. Observability and bounded-efficiency proof

Extend existing internal/test statistics rather than adding user-facing
configuration. Count at least:

- unique chunks selected as remote, sealed prepared, segment, and newly
  claimed;
- claim wins/losses and SQLite statements/batch sizes;
- compression input bytes versus unique newly prepared bytes;
- local prepared payload bytes/files created and reused;
- prepared payload opens/bytes and maximum concurrent readers;
- xorb upload attempts/bytes and remote object requests;
- peak recipe page, claim batch, open files, temp bytes, and worker counts.

Assertions use counts and byte bounds, not wall-clock thresholds. No new env
var or configuration knob is permitted unless the existing bounded worker,
page, and payload permits provably cannot express the limit; stop for design
review first.

## Commands you will need

Verify `/Volumes/Workspace` is mounted and create/use only this checkout's
target directory. Set `CARGO_TARGET_DIR` on every compiling invocation.

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Staging | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked` | all pass |
| Add | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cmd::add::` | all pass |
| Prepared push | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked add_time_push_plan -- --nocapture` | all pass |
| CLI E2E | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --test e2e_add_commit_push --locked` | all pass |
| RustFS smoke | Build the release binary in the external target, then run `python3 crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py --bucket "$BUCKET" --endpoint-url "$ENDPOINT" --crab-bin "$CARGO_TARGET_DIR/release/crab" --root "$RUN_ROOT" --run-id "$RUN_ID" --size-mib 4` | add/commit/push/clone/hydrate and evidence validation pass |

Do not print credentials. Source the ignored `.env` only in the dedicated
RustFS/provider environment.

## Scope

**In scope**:

- `crates/crab-staging/src/index.rs`
- `crates/crab-staging/src/lib.rs`
- `crates/crab-staging/src/stream.rs`
- `crates/crab-staging/src/add_push_plan.rs`
- `crates/crab-staging/src/push_plan.rs`
- `crates/crab-staging/src/stats.rs`
- `crab/src/cmd/add.rs`
- `crab/src/git/push.rs`
- adjacent unit/property tests in those modules
- `crab/tests/e2e_add_commit_push.rs`
- `crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py`
- `.github/workflows/pb-provider-qualification.yml` if the existing workflow
  needs a partial-overlap mode wired into retained qualification evidence
- `crab/docs/design/add.md`
- `crab/docs/design/push.md`
- `crab/docs/guides/staging.md`
- `crab/src/cmd/stat.rs`, `crab/src/cmd/doctor.rs`,
  `crab/schemas/stat.push-plan.json`, and `crab/tests/schema_validate.rs` only
  if the canonical diagnostics contract needs to expose the new authority
  counts or old-database remediation

**Out of scope**:

- remote `.crab/xorbs/` layout or xorb binary format;
- compression algorithm, target xorb size, chunker, or hash identity;
- bucket-global SlateDB schema beyond consuming the current committed receipt;
- uploading during add or making add network-dependent beyond optional remote
  proof lookup;
- shard partitioning, Git pointer/protocol shape, GC policy, or provider
  admission;
- a config/env flag, v2 name, compatibility reader, migration, or dual path.

## Git workflow

- Branch: `codex/016-canonical-prepared-authority`
- Keep commits behaviorally reviewable: schema/invariants; preparation claims;
  content-addressed sealing/recovery; normalized push consumer; E2E/docs.
- Example commit: `refactor: canonicalize prepared chunk authority`.
- Rebase on the latest `origin/main`; do not merge `origin/main` into the
  branch. Preserve unrelated user changes.

## Implementation steps

### Step 1: Lock the failure and efficiency properties with tests

Add a direct CLI-path fixture containing two files above the direct-stream
threshold whose content is partially, but not exactly, overlapping. Ensure the
overlap crosses multiple chunks and builder flush boundaries. Run with at least
two direct builders and adversarial scheduling.

Pre-cutover characterization must demonstrate the current conflict or duplicate
placement without weakening the eventual assertion. Final assertions:

- one prepared placement per unique shared new chunk;
- one local payload file per xorb hash and no per-file copies;
- both recipes are fully covered;
- push uploads each selected xorb at most once;
- fresh clone/hydrate is byte-identical.

Add counter hooks before refactoring so Steps 2-6 prove reduced work.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked direct_stream_partial_overlap -- --nocapture`
→ the regression passes with deterministic uniqueness/byte counters.

### Step 2: Hard-cut the normalized schema and payload layout

Implement `prepared_payload_chunks` uniqueness and preparation/claim tables.
Move payload path construction to the content-addressed sharded layout. Replace
per-file plan persistence and queries with normalized payload, placement,
remote, segment, and lease queries. Delete the old schema and serializers in
the same commit; increment only the strict v1 schema fingerprint/identity used
to reject disposable old state.

Add schema tests for unique placement, foreign-key cleanup, lease protection,
old-state rejection, and deterministic path derivation. Do not add migration
tests.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked prepared_payload`
→ duplicate chunk placement is rejected and lease/reclamation behavior passes.

### Step 3: Add bounded preparation-wide chunk claiming

Open one preparation for the whole add publication. Implement batched authority
resolution and atomic claims. Feed only claim winners to builders; persist
non-owner occurrences without buffering their bytes after verification. Keep
current worker/build concurrency and make cancellation propagate to the whole
preparation.

Exercise exact duplicates, repeated hashes within one file, partial overlaps,
multiple builder flushes, randomized file order, worker cancellation, and two
sequential `crab add` commands. Separate add processes remain serialized by the
staging writer lock; prove the second add reuses sealed authority.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked prepared_chunk_claim`
→ one claim winner and one eventual placement exist for every unique new hash.

### Step 4: Seal payloads and finalize all recipes atomically

Implement temp-write/verify/fsync/rename and one whole-preparation SQLite
finalization. Resolve every dependent occurrence, assert exact authority
coverage, then hand the sealed recipe set to the existing publication intent.
On any failure, roll back every member batch and preserve prior heads/index.

Inject failures at claim insertion, builder seal, payload write, rename,
SQLite finalization, publication-intent creation, and Git index replacement.
Reopen staging after each failure and assert one converged state with no live
claim, partial recipe, premature head, or leased orphan.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked add_preparation_recovery -- --nocapture`
→ every injected interruption converges and exact coverage holds.

### Step 5: Derive push authority from normalized snapshot rows

Replace file-plan JSON loading/merging with bounded normalized queries pinned by
the existing push snapshot. Deduplicate xorb validation and upload by hash.
Retain strict full-payload/footer/chunk/placement checks and the conflict guard.
Allow segment repack only when independent verified segment coverage exists.

Test mixed remote/prepared/segment recipes, stale remote proof, missing/corrupt
prepared bodies, repeated chunks, shared payloads, restage during an open push,
retry/CAS replan, and protected multipart upload. A prepared-only corrupt body
must fail before any ref advances.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked add_time_push_plan -- --nocapture`
→ authority is complete, each source opens/uploads once, and corruption fails closed.

### Step 6: Prove scale bounds and end-to-end behavior

Add a property test generating overlap graphs across N files. For every seed:

- each unique newly prepared chunk has exactly one placement;
- every recipe occurrence resolves to exactly one selected authority;
- prepared/compressed bytes are proportional to unique new bytes;
- reconstruction is byte-identical;
- RSS proxies, claim batches, recipe pages, open payloads, workers, and temp
  files stay under explicit existing bounds.

Extend the real CLI E2E and RustFS harness with partial-overlap, sequential add,
restage-before-push, and concurrent push/restage cases. Retain evidence counters
for unique bytes, payload files, payload opens, uploads, object requests, and
fresh hydration digest.

**Verify**: run all commands in **Commands you will need**. Broad and RustFS
proof belongs in CI/dedicated environments when credentials/services are not
available locally.

### Step 7: Update diagnostics and documentation

Document the four-way authority decision, local versus remote paths, strict v1
state rejection, restage recovery, and why bucket-global SlateDB never contains
pending claims. Update stat/doctor output only if it gives an actionable answer
for unresolved preparations, corrupt payloads, or old staging state; keep JSON
schema/tests synchronized.

Delete documentation of per-file prepared plans and fallback behavior.

**Verify**:
`rg -n "file_push_plans|prepared_xorbs|push-plans/xorbs" crab crates packages/web plans --glob '!plans/016-canonical-prepared-chunk-authority.md'`
→ no live code or current docs depend on retired ownership; historical plans
may remain as delivery records.

## Test matrix

| Case | Required proof |
|---|---|
| Two large partially overlapping new files in one add | One placement per shared hash; successful RustFS push and exact hydrate. |
| N files/random overlap graph | Property holds across scheduling/order seeds with bounded counters. |
| Exact duplicate files and repeated hash in one file | One physical placement; every occurrence reconstructs. |
| Two sequential add invocations before push | Second add reuses sealed local prepared authority. |
| Valid committed cross-repo receipt | No segment, prepared payload, compression, or upload for the hit. |
| Existing sealed prepared hit | One new lease; no new body or compression. |
| Existing segment hit | No add-time repack; push retains bounded canonical packing. |
| Cancellation/crash at every lifecycle boundary | Old heads/index remain or exact new publication reconciles; no partial claims/leases. |
| Restage while push snapshot is open | Old payload stays pinned until snapshot retirement; new head is independent. |
| Missing/corrupt prepared-only body | Push fails closed before remote publication with actionable restage guidance. |
| Missing/corrupt body with verified segment authority | Canonical bounded repack succeeds without arbitrary placement choice. |
| Offline add | Local authority selection and publication succeed without object-store availability unless remote-only proof is required. |
| CAS retry/protected push | No duplicate body upload; metadata and staged mapping remain complete. |

## Acceptance criteria

- [x] Direct streaming cannot create two live prepared placements for one chunk hash.
- [x] Multi-file partial overlap can add, commit, push, clone, and hydrate byte-identically on RustFS.
- [x] Every published recipe occurrence has exactly one selected remote, prepared, or segment authority.
- [x] Unknown bytes are compressed and stored once even across files and sequential adds.
- [x] Direct-prepared chunks do not require duplicate segment payloads.
- [x] Prepared bodies are local content-addressed files; bucket xorbs are written only by push.
- [x] Bucket-global SlateDB contains committed remote receipts only and is updated post-CAS.
- [x] Restage, cancellation, crash recovery, and push snapshots preserve exact leases and reclaim orphans.
- [x] Push validates/opens/uploads each unique prepared payload at most once per attempt.
- [x] No v2, compatibility, migration, alias, dual read/write, fallback reader, or new config flag remains.
- [x] Counter/property tests prove work and memory are bounded by unique data and existing limits.
- [x] Focused, full E2E, and retained RustFS qualification are green.
- [x] Add/push/staging documentation describes the single v1 design.

## Rejected alternatives

- **Put provisional ownership in bucket-global SlateDB**: rejects offline add,
  advertises data other clients cannot read, creates bucket-wide writer
  contention, and requires distributed leases/TTL/tombstones. Keep it
  committed-only.
- **Disable direct preparation for multi-file add**: correct but loses one-pass
  preparation, increases segment writes and later reads, and leaves sequential
  add deduplication unresolved.
- **Use an in-memory global hash map**: bounds neither PB-scale memory nor crash
  recovery and cannot protect future call sites. SQLite owns the invariant.
- **Let push choose an arbitrary conflicting xorb**: hides producer corruption,
  makes results order-dependent, and can upload redundant bodies.
- **Retain per-file hardlinks or copied plan JSON**: duplicates lifecycle and
  cleanup state without adding authority; normalized leases already express
  many-to-one ownership.
- **Always write both segment and prepared payload**: creates a fallback by
  doubling local writes/storage on the hottest path. Fail closed when the sole
  verified authority is corrupt.
- **Upload during add**: makes add network-dependent, creates unreferenced remote
  garbage on restage/failure, and crosses Plan 011's publication boundary.

## STOP conditions

Stop and report before continuing if:

- the implementation needs a bucket-visible provisional claim or add-time
  upload for correctness;
- whole-preparation atomicity cannot compose with Plan 011 publication intents
  and Plan 012 leases without a second lifecycle owner;
- the xorb writer cannot produce a fully verified body before recipe
  publication without unbounded buffering;
- a proposed fallback would reconstruct from unverified bytes or weaken full
  payload/footer/chunk/placement validation;
- the only implementation introduces a second path, v2 name, migration, or
  compatibility reader;
- `/Volumes/Workspace` is unavailable for compiling verification;
- unrelated user changes overlap a required file and cannot be preserved.

## Maintenance notes

Treat `prepared_payload_chunks.chunk_hash UNIQUE` as a reconstruction safety
invariant, not merely a performance index. Any future repack/restripe path that
wants a second placement must first retire or atomically replace the canonical
local authority and update all leases/snapshots; it must not bypass the table.

The bucket-global chunk index remains rebuildable acceleration. Correctness
continues to come from origin-bound committed manifests/shards/receipts and
locally verified payloads. Never weaken remote-proof validation to save a GET.
