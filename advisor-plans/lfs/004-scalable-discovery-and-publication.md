# Phase 3: Make pointer discovery and pre-ref publication scale

> **Executor instructions**: Preserve the invariant that every reachable LFS object is durable before any ref becomes visible. Update the Phase 3 row on completion.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crab/src/cmd/lfs/mod.rs crab/src/cmd/lfs/push.rs crab/src/cmd/lfs/dedup.rs crab/src/lfs/publication.rs crab/src/git/push.rs crates/crab-git`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: Phases 1 and 2
- **Category**: perf, correctness, architecture
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

The current push scanner captures complete `rev-list` and `cat-file` output, and the common push gate rescans and revalidates each object. Live qualification transferred LFS payloads quickly but spent roughly eight minutes in the remaining push/publication path. This phase makes work proportional to bounded batches while retaining the object-before-ref safety boundary.

## Current state

- `crab/src/cmd/lfs/push.rs:337` captures all `git rev-list --objects` output, builds all hashes, and captures all `git cat-file --batch` output.
- `crab/src/cmd/lfs/push.rs:356` falls back from a failed requested-range walk to unrelated `HEAD` and drops paths, which can omit pushed dependencies and lock checks.
- `crab/src/cmd/lfs/mod.rs:1003` discards the pre-push remote name/URL and `crab/src/cmd/lfs/push.rs:219` resolves `None`, so LFS publication can target a different remote than the Git ref push.
- The existing test at `crab/src/cmd/lfs/push.rs:767` proves input beyond 64 KiB, not bounded memory.
- `crab/src/cmd/lfs/dedup.rs` already streams revision output, batches 500 objects, checks object type/size, caps records/output, and validates response counts.
- `crab/src/lfs/publication.rs:15` rescans all tips, checks locks, then sequentially verifies/uploads/reverifies objects.
- `crab/src/git/push.rs:15320` calls publication before ref publication. This owner boundary must remain.

## Commands you will need

| Purpose | Command | Expected |
|---------|---------|----------|
| Push/LFS tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib 'lfs::publication\|cmd::lfs::push' --locked --no-default-features` | all selected tests pass |
| Broad LFS | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib lfs --locked --no-default-features` | all pass |

## Scope

**In scope**:
- bounded Git-object enumeration in `crates/crab-git` if shared by multiple current callers, otherwise `crab/src/cmd/lfs/push.rs`
- `crab/src/cmd/lfs/dedup.rs` only to extract/delete duplicate mechanics
- `crab/src/cmd/lfs/mod.rs` pre-push dispatch
- `crab/src/lfs/publication.rs`
- `crab/src/git/push.rs`
- focused push tests

**Out of scope**:
- altering Git ref coordination semantics
- trusting a pre-push hook as the sole durability gate
- persistent cross-push caches without remote identity/validator binding
- remote GC

## Git workflow

- Branch: `advisor/lfs-phase-3-publication`
- Keep commits bisectable: scanner, manifest, publication switch, old-path deletion.

## Steps

### Step 1: Create one bounded pointer walker

Extract the proven dedup pattern into the correct Git-mechanics owner. Stream `rev-list`, batch `cat-file --batch-check`, reject non-blobs and oversized pointer candidates before body reads, batch `cat-file --batch`, cap object count/record length/output bytes, preserve paths with spaces, and detect count/protocol mismatches. The visitor should emit entries incrementally. All requested-range walk, subprocess, parse, and body-read failures must fail closed. Delete the fallback-to-`HEAD` behavior; never substitute another revision scope.

**Verify**: a synthetic 100,000-object history test reports a fixed maximum batch/output allocation and finds every LFS pointer once.

### Step 2: Bind publication to the selected Git remote

Thread the pre-push remote name and URL through dispatch and resolve the exact repository identity/prefix selected by Git. Reject unknown, conflicting, or non-Crab remote information. The custom-agent init path follows the same rule in Phase 2. Add a two-remote fixture with different prefixes.

**Verify**: pushing each named remote stores dependencies only under that remote's prefix; a mismatched URL aborts before upload/ref mutation.

### Step 3: Build a push-scoped dependency manifest

At the common push entry point, collect unique OID/size plus every path needed for lock checks. Reject one OID with conflicting sizes. Bind the manifest to remote identity, repository prefix, old/new tips, and operation ID. Keep it in memory for a single push unless a cross-process handoff is explicitly needed.

**Verify**: tests reject changed tips, changed remote identity, conflicting sizes, missing paths, and malformed pointers.

### Step 4: Publish through the canonical coordinator

Check lock conflicts before upload admission. Submit unique missing/invalid objects to the Phase 2 coordinator. Produce a publication receipt listing manifest digest, remote identity, object count/bytes, and successful completion. Ref publication consumes only a receipt created in the current operation.

**Verify**: fault-injection tests show any missing/corrupt/upload-failed object prevents the ref update; all-valid objects allow it.

### Step 5: Remove duplicate scans and payload verification

Use the same dependency manifest throughout native and remote-helper push composition. If a standalone pre-push process completed earlier, treat its receipt only as an optimization after validating Phase 4 remote receipts; never skip the in-process durability gate based on a local file alone.

**Verify**: instrumentation on a 100-commit push reports one reachability walk and no successful-object body downloads during publication.

### Step 6: Add scale and cancellation tests

Cover 100 commits, 1,000 current paths, 100,000 distinct pointers, duplicate OIDs, deletion/rename histories, force-push ranges, zero old OID, multiple tips, and cancellation during scan/upload. Assert bounded memory and no ref visibility after cancellation.

**Verify**: all selected and broad LFS tests pass; scale test records one scanner process pair rather than per-object subprocesses.

## Test plan

- Reuse real temporary Git repositories; do not mock `rev-list`/`cat-file` wire formats in the main integration proof.
- Add parser-level fixtures for NUL-safe paths, malformed/truncated records, non-blobs, oversized blobs, response-count mismatch, and child failure.
- Add two-remote, multi-tip, non-HEAD, force-push, rename/delete, conflicting-size, cancellation, and ref-race tests.
- Instrument scans, body GETs, metadata requests, uploads, and ref updates so acceptance is machine-checkable.

## Acceptance criteria

- [ ] Pointer discovery is streamed and batch-bounded for 100,000+ objects.
- [ ] A failed requested-range scan aborts before object mutation or ref publication; it never falls back to `HEAD`.
- [ ] Each push performs one canonical reachability walk.
- [ ] LFS dependencies and Git refs resolve to the same selected remote identity and repository prefix.
- [ ] Lock checks and all object publication complete before ref visibility.
- [ ] Publication uses the Phase 2 coordinator and does not sequentially full-verify each payload.
- [ ] A receipt cannot be replayed for another remote, prefix, tip set, or operation.
- [ ] Failure/cancellation tests prove no partially durable push publishes refs.

## STOP conditions

- Ref publication has another entry point that bypasses `crab/src/git/push.rs` and cannot consume the manifest.
- The proposed manifest omits paths required for lock enforcement.
- Avoiding a second scan would require trusting an unsigned/stale cross-process file.
- Extraction into `crab-git` would expose product policy rather than reusable Git mechanics.

## Maintenance notes

This is the highest-risk phase. Review the entire push entry point, coordinator callee, ref commit boundary, protected-push sibling path, and tests before approval.
