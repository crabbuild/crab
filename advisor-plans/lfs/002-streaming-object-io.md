# Phase 1: Make LFS object I/O streaming, resumable, and memory-bounded

> **Executor instructions**: Implement reusable mechanics in `crab-lfs`; keep CLI policy in `crab`. Run every gate and update the Phase 1 index row.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crates/crab-lfs crates/crab-storage crates/crab-auth-server/src/view.rs crab/src/lfs/cache.rs crab/src/lfs/transfer_agent.rs crab/src/lfs/batch.rs crab/src/git/filter_process.rs crab/src/cmd/lfs/standalone.rs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: Phase 0
- **Category**: perf, correctness
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

Current large uploads are bounded, but downloads and corruption repair can materialize the full object. A production LFS implementation must handle multi-gigabyte objects with memory bounded by configured buffers, verify every byte once, resume safely, and install atomically.

## Current state

- `crates/crab-lfs/src/object_store.rs:221` provides bounded multipart upload, but its corrupt-repair branch uses `tokio::fs::read`.
- `crates/crab-lfs/src/object_store.rs:290` returns complete objects as `Bytes`; `verify` returns the same full payload.
- `crab/src/lfs/batch.rs:236` downloads via full `Bytes` and `install_bytes` hashes again.
- `crab/src/lfs/transfer_agent.rs:490` selects a “large” resume path at 64 MiB, but range responses and the completed partial are still materialized.
- `crab/src/lfs/cache.rs:23` already has a temp-file writer and atomic install shape; extend this ownership instead of inventing another cache layout.
- `crates/crab-lfs/src/object_store.rs:290` chooses a replica before SHA-256 verification, so corrupt replica bytes fail instead of retrying a valid primary.
- Invariant: reconstruction/download is byte-identical or errors. Never expose a partially verified file.

## Commands you will need

| Purpose | Command | Expected |
|---------|---------|----------|
| Unit tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab-lfs --locked` | all pass |
| CLI LFS tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib lfs --locked --no-default-features` | all pass |
| Check | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo check -p crab --locked --no-default-features` | exit 0 |

## Scope

**In scope**:
- `crates/crab-lfs/src/object_store.rs`
- `crates/crab-lfs/src/lib.rs` and focused tests
- only the minimal streaming/range seam in `crates/crab-storage/src/store.rs`
- `crab/src/lfs/cache.rs`
- adapters in `crab/src/lfs/transfer_agent.rs` and `crab/src/lfs/batch.rs`
- LFS output adapters in `crab/src/git/filter_process.rs` and `crab/src/cmd/lfs/standalone.rs`
- LFS copy path in `crates/crab-auth-server/src/view.rs`

**Out of scope**:
- changing `{prefix}/lfs/objects/{aa}/{bb}/{oid}`
- remote presence receipts; Phase 4 owns them
- HTTP LFS APIs
- weakening legacy-object verification

## Git workflow

- Branch: `advisor/lfs-phase-1-streaming`
- Prefer two commits: shared storage mechanics, then CLI adapters/tests.
- Do not push unless instructed.

## Steps

### Step 1: Define a streaming download contract

Add a narrow `LfsObjectStore` method that streams an expected OID and size into a caller-owned async sink or file. It must incrementally SHA-256, count bytes with overflow checks, fail on truncated/oversized bodies, support an optional verified resume offset, and return metadata rather than payload bytes. Keep `get` only for genuinely small existing callers.

**Verify**: `rg -n "pub async fn.*stream|expected_size|resume" crates/crab-lfs/src/object_store.rs` finds the new contract; `cargo test -p crab-lfs` passes.

### Step 2: Make cache staging async and atomic

Adapt the cache temp writer so the streamed body lands beside `.git/lfs/objects`, is synced, verified, then atomically persisted. Give each attempt a unique temp file. Persist resume metadata containing remote identity, OID, expected size, verified prefix length, and validator; do not trust a partial file based only on its filename or length.

**Verify**: tests prove failed, truncated, and corrupt downloads leave no final object and a valid retry installs one final object.

### Step 3: Replace payload-materializing download paths

Switch `BatchResolver` and the custom transfer agent to the streaming primitive. Remove full range-response retention and the completed-partial reread. Progress must be based on bytes durably written. Linked worktrees sharing the common Git directory must not write the same temp path.

Make verified read own replica selection: retry primary only for selected-replica missing or integrity failure, never for authorization/configuration failures. Record fallback reason without hiding replica corruption.

After cache verification, stream the file to standalone/filter output without another full `Vec`/`Bytes` copy. Extension transforms that cannot stream must spool to bounded temporary storage or fail with a documented limit. Make protected-view LFS copies use storage-side copy when it preserves validator/receipt semantics, otherwise use the same bounded stream.

**Verify**: `rg -n "store\.verify\(|store\.get\(" crab/src/lfs/batch.rs crab/src/lfs/transfer_agent.rs` shows no full-payload download in transfer paths.

### Step 4: Bound the corrupt remote repair path

Do not replace a corrupt immutable object with an unbounded `Bytes` payload. Implement a provider-neutral staged replacement or a verified multipart upload followed by conditional publication. Preserve race safety: a concurrently repaired valid object must never be overwritten by corrupt or unverified bytes.

**Verify**: tests inject a corrupt existing object and concurrent valid writer; final remote bytes match the OID and memory remains bounded.

### Step 5: Add memory and interruption tests

Test sizes 0, 1 byte, 1 MiB, 64 MiB minus/at/plus one, multipart boundary minus/at/plus one, and a sparse/generated multi-gigabyte stream. Add kill/restart and wrong-resume-validator cases. Instrument allocated/in-flight buffer bytes rather than relying only on host RSS.

**Verify**: maximum in-flight payload bytes stay below a documented constant independent of object size; all integrity tests pass.

## Test plan

- Extend `crates/crab-lfs/src/object_store.rs` tests with counting/chunked stores for truncation, overflow, corrupt replica, primary fallback, resume validator changes, cancellation, and multipart repair races.
- Extend cache/agent/filter tests for unique temporary state, linked Worktrees, atomic install, streaming output, and extension spooling.
- Add a protected-view copy test proving byte identity without full-payload APIs.
- Run both commands in “Commands you will need”; no new test may depend on live credentials.

## Acceptance criteria

- [ ] Upload, download, resume, and corrupt repair use bounded buffers for arbitrarily large objects.
- [ ] Each successfully installed object is size-checked and SHA-256 verified exactly once on the download path.
- [ ] Partial state is namespaced and validator-bound; linked worktrees cannot collide.
- [ ] Cancellation or failure never exposes a final unverified object.
- [ ] A corrupt/missing selected replica can fall back to a valid primary; auth/config errors fail without fallback.
- [ ] Existing `get`/`verify` callers are inventoried; any retained full-payload caller has an explicit small-object bound.

## STOP conditions

- `object_store` cannot expose a provider-neutral streamed/ranged body with validator metadata.
- Atomic install would require changing the standard Git LFS local object layout.
- A proposed repair path can overwrite a newer valid object without a conditional check.
- An in-scope public API change has unreviewed callers outside the scope list.

## Maintenance notes

Reviewers should scrutinize cancellation, sync/rename ordering, validator changes, and multipart abort. Do not infer integrity from ETag; SHA-256 remains authoritative.
