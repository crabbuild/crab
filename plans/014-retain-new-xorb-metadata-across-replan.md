# Plan 014: Retain full new-xorb metadata across CAS replans

> **Executor instructions**: Keep file reachability ref-scoped while retaining
> complete immutable metadata for every newly packed xorb. Do not solve sparse
> placement failures by weakening the dense-index guard. Run all gates and
> update `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 86139f55..HEAD -- crab/src/git/push.rs crates/crab-xet/src/xorb/builder.rs crates/crab-xet/src/shard.rs crab/tests`
> If CAS replanning, placement ownership, or packed-xorb result types changed,
> reconcile the plan before editing.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: `plans/013-dependency-closed-shard-partitioning.md`
- **Category**: bug
- **Planned at**: commit `86139f55`, 2026-08-29
- **Delivery status**: DONE — the real shared-xorb non-atomic CAS regression,
  focused push/shard tests, broad library suite, and RustFS acceptance pass;
  delivered in the combined v1 hardening change set.

## Why this matters

A non-atomic multi-ref push may pack chunks from several file runs into one
immutable xorb. If one ref loses a manifest-CAS race, replanning filters the
original placement map to surviving files but does not repack or retain full
metadata for that already-uploaded xorb. When the surviving chunks begin at a
non-zero index or leave holes, shard construction fails and rejects an
otherwise unconflicted ref.

## Current state

- `crates/crab-xet/src/xorb/builder.rs:576-600` can keep different run IDs in
  one xorb while it remains below the target; file run boundaries do not imply
  one xorb per file.
- `crab/src/git/push.rs:14069-14098` re-enumerates, looks up, classifies, and
  rebuilds shards after a CAS conflict, but it does not repack xorbs.
- `crab/src/git/push.rs:12131-12149` filters `chunk_placement` to chunks required
  by surviving files.
- `crab/src/git/push.rs:12217-12233` derives new-xorb metadata from that filtered
  subset unless the xorb came from verified-existing metadata.
- `crab/src/git/push.rs:1406-1428` correctly requires dense zero-based indices
  when constructing a complete `MDBXorbInfo`; do not weaken this invariant.
- Existing test `crab/src/git/push.rs:29337-29425` gives the kept and removed
  files separate one-chunk xorbs, so it cannot reproduce shared-xorb sparsity.
- `verified_existing_xorb_info` is the sibling ownership model: complete xorb
  metadata is stored separately from ref-scoped placement selection.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Push focused tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked non_atomic -- --nocapture` | all matching tests pass |
| Shard focused tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked build_shard -- --nocapture` | all matching tests pass |

## Scope

**In scope**:

- `crab/src/git/push.rs`
- adjacent push unit/integration tests
- `crates/crab-xet/src/xorb/builder.rs` only if a test helper is needed to
  deterministically create a shared xorb; do not change packing policy

**Out of scope**:

- Repacking immutable xorbs after CAS conflict.
- Including removed files or their file terms in the rebuilt generation.
- Weakening dense xorb metadata validation.
- Changing atomic-push semantics, manifest CAS, xorb hash format, or shard
  partition policy.
- Persisting new remote metadata formats.

Version rule: this plan introduces no new serialized version. If completing the
metadata ownership requires changing a Crab-owned serialized contract, hard-cut
that contract in place as v1 and delete its obsolete reader/writer; do not add
v2 or migration code.

## Git workflow

- Branch: `advisor/014-retain-replan-xorb-metadata`
- Suggested commits: `fix: retain packed xorb metadata for replan`, then
  `test: cover shared-xorb non-atomic retry`.
- Read `crates/AGENTS.md` before any shared-crate test-helper edit.

## Steps

### Step 1: Add the missing shared-xorb CAS-conflict regression

Construct two pointer files whose chunks are packed into one xorb under
distinct `RunId`s. Exercise both removal shapes:

- removed ref owns prefix chunk(s), survivor owns a non-zero suffix;
- survivor owns prefix and removed ref owns suffix;
- optional third case leaves an interior hole.

Trigger the real non-atomic replan path, not only
`build_xorb_info_from_placements`. Assert that the survivor commits, its shard
contains only the surviving file term, and its referenced xorb info is the
complete ordered metadata of the immutable uploaded xorb. The removed file
must not enter file index, shard file entries, receipts, or cleanup snapshots.

Use `build_shard_excludes_dependencies_for_removed_non_atomic_ref` as structure
but replace its two-xorb fixture with one real shared-xorb builder result.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked shared_xorb_non_atomic_replan -- --nocapture`
→ the new test reproduces the pre-fix failure and passes after Steps 2-3.

### Step 2: Capture complete metadata when each new xorb is packed

Add one pipeline-owned map keyed by xorb hash that stores full ordered
`MDBXorbInfo` (or an equivalent complete immutable placement sequence) for
newly packed xorbs. Populate it at the packing boundary from the complete
`XorbResult` before any ref-scoped filtering. Validate:

- all indices are dense and zero-based;
- the xorb hash and placement xorb hashes agree;
- chunk hashes/sizes and byte offsets are representable;
- duplicate insertion for one hash is byte-for-byte metadata-identical.

Do not derive this map later from `chunk_placement`, because that map is
intentionally filtered during replanning. Keep verified-existing and newly
packed metadata distinguishable if their proof/lifecycle differs, but expose
one read-only lookup to shard construction.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked packed_xorb_info`
→ complete, duplicate-identical, conflict, and index validation tests pass.

### Step 3: Build surviving shards from ref-scoped terms plus full xorb info

Keep `required_chunks`, `merged_placement`, and file reconstruction terms
limited to surviving pointer files. When any surviving placement references an
xorb, obtain its complete metadata from either verified-existing info or the
newly packed map. Never rebuild its metadata from the filtered subset.

An xorb referenced only by a removed ref must not enter the shard. An xorb
shared with a surviving ref may include metadata for chunks no longer used by
a file term; that is correct because the immutable xorb itself remains a
surviving dependency. Preserve the dense guard for any fallback construction
path and fail on conflicting metadata.

Supply this full metadata through Plan 013's dependency-closed file bundle API.
Keep partition ownership in that API; this plan owns only complete metadata
retention and ref-scoped selection.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked build_shard_excludes`
→ all matching tests pass. Then run
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked shared_xorb_non_atomic_replan`
→ all new shared-xorb cases pass.

### Step 4: Preserve cleanup and retry ownership

Audit reset/clear points for the new map. It must survive a non-atomic CAS
replan within the same pipeline, but be released when the pipeline ends or a
new independent push pipeline begins. It must not make an uploaded removed-only
xorb a manifest dependency; ordinary orphan/GC policy owns that immutable
object.

Add assertions that retry does not upload the shared xorb again and that
post-success cleanup retires only the surviving snapshot leases.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cas_dependency_replan`
→ retry/upload-count and cleanup assertions pass.

## Test plan

- Regression: prefix removal, suffix removal, and interior hole in one shared
  newly uploaded xorb.
- Integrity: conflicting full metadata for the same xorb hash fails before
  shard upload.
- Reachability: removed file terms/receipts absent; shared full xorb info
  present only because a survivor references the xorb.
- Retry: immutable xorb uploaded once; rebuilt shard and file index succeed.

## Done criteria

- [x] Shared-xorb non-atomic CAS retry commits every unconflicted surviving ref.
- [x] New xorb metadata is captured completely before ref-scoped filtering.
- [x] Dense-index validation remains unchanged or stronger.
- [x] Removed file terms and removed-only xorbs do not enter the rebuilt generation.
- [x] Shared immutable xorbs are not reuploaded during replan.
- [x] Focused push/shard, format, broad library, and RustFS checks pass.
- [x] The combined v1 hardening change set is committed locally; CAS replan
  consumes the same complete new-xorb metadata as initial shard construction.

## STOP conditions

Stop and report if:

- Xorb identity cannot be reconstructed exactly from the packing result.
- Full metadata retention would require retaining compressed payload bytes.
- A consumer interprets extra unused chunk metadata inside a referenced xorb
  as reachability of removed files.
- The failure reproduces only by changing production packing policy.
- Fixing it requires changing manifest CAS or atomic-push semantics.

## Maintenance notes

File terms are ref-scoped; xorb metadata describes the complete immutable
object. Never derive the latter from a filtered view of the former. Reviewers
should inspect reset points and prove the metadata map survives exactly one
pipeline's CAS retries without becoming a cross-push cache.
