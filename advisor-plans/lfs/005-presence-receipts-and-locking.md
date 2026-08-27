# Phase 4: Add trusted remote presence receipts and race-safe locking

> **Executor instructions**: Never infer SHA-256 from provider ETag. Preserve streamed verification for legacy or untrusted state. Update the Phase 4 row when complete.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crates/crab-lfs crates/crab-storage crab/src/lfs/lock.rs crab/src/cmd/lfs/locks.rs crab/src/lfs/publication.rs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: Phases 2 and 3
- **Category**: perf, correctness, security
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

Content-addressing makes repeated payload verification unnecessary only when Crab can prove which bytes were previously verified and committed. A trusted receipt makes presence checks cheap without weakening legacy integrity. Production locking also needs an owner/token-aware state transition so unlock cannot erase a newer lock.

## Current state

- `crates/crab-lfs/src/object_store.rs:431` streams and hashes the complete existing object before every put.
- `crates/crab-storage/src/store.rs:1103` HEAD returns size, ETag, and version but no authoritative SHA-256 metadata.
- S3 multipart ETag is not SHA-256; GCS/Azure validators have different semantics.
- `crab/src/lfs/lock.rs:155` documents a read-then-delete TOCTOU window because the current object-store trait has no conditional delete.
- Lock records are Crab JSON under `{prefix}/lfs/locks/{blake3(path)}`; the CLI is the supported compatibility surface.
- `crab/src/cmd/lfs/locks.rs:51` hashes raw CLI paths while push checks Git-produced repository-relative paths, so aliases and paths outside the Worktree can create a different lock identity.
- `crab/src/cmd/lfs/store_setup.rs:303` uses mutable Git author identity as lock owner; managed authorization needs an authenticated principal instead.
- `crab/src/cmd/lfs/locks.rs:472` updates shared local lock caches with non-atomic read/modify/write operations.

## Commands you will need

| Purpose | Command | Expected |
|---------|---------|----------|
| Shared crates | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab-storage -p crab-lfs --locked` | all pass |
| Lock tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib 'lfs::lock\|cmd::lfs::locks\|lfs::publication' --locked --no-default-features` | all selected tests pass |

## Scope

**In scope**:
- a versioned LFS verification receipt/manifest in `crates/crab-lfs`
- minimal CAS/storage primitives in `crates/crab-storage`
- receipt consumption in transfer/publication
- lock state transitions and tests
- one canonical repository-relative Git path type and atomic local lock-cache update

**Out of scope**:
- provider ETag-as-digest shortcuts
- standard HTTP request/response models; an external LFS server owns that surface
- bucket-wide deletion or GC
- silently backfilling receipts without hashing legacy payloads

## Git workflow

- Branch: `advisor/lfs-phase-4-receipts-locks`
- Separate receipt/layout and lock-state commits. Serialized layouts require explicit review.

## Steps

### Step 1: Specify a versioned verification receipt

Store a small immutable or CAS-updated record keyed by repository prefix and OID. Include schema version, OID, size, object path, provider object version/validator, verified-at time, and verifier version. Document that the receipt is trusted only when path, size, and current provider version/validator match. If provider versioning is unavailable, define a safe committed-generation token created by Crab; otherwise fall back to payload verification.

**Verify**: serialization golden tests lock the schema and reject unknown/incomplete/cross-prefix records.

### Step 2: Commit object and receipt with recoverable ordering

Upload and SHA-256 verify bytes first, then publish the receipt. A receipt must never precede durable bytes. A crash between object and receipt causes only a later full verification; a crash must never create a false positive. Corrupt repair invalidates/replaces the receipt conditionally.

**Verify**: crash-point tests at every transition produce either no trusted receipt or a valid object/receipt pair.

### Step 3: Use a three-state presence result

Return `missing`, `present_trusted`, or `present_needs_verification`. HEAD/receipt checks may accept only `present_trusted`. Legacy/mismatched receipts stream and hash the body, then backfill a receipt. Batch operations use the existing bounded HEAD/LIST patterns without listing unrelated repository data.

**Verify**: publication of 10,000 already-verified objects performs bounded metadata requests and zero payload GET bytes; legacy objects are each streamed once.

### Step 4: Replace lock deletion with a token-safe transition

Canonicalize every lock target to one repository-relative Git path before remote access; reject paths outside the Worktree and normalize equivalent aliases. In managed mode bind ownership/force permission to the authenticated principal, not mutable `user.email`; direct-storage identity remains explicitly advisory. Because the dependency lacks conditional delete, use a CAS-updated terminal/tombstone record or another provider-neutral state machine. Unlock must present the lock ID and owner unless authorized force is used; a stale unlock cannot remove or invalidate a later lock. Expiry replacement must remain CAS-protected. Compact tombstones only under a separate safe retention rule.

**Verify**: deterministic races cover unlock versus force-unlock/relock, expiry replacement, duplicate owner acquisition, stale ID, and concurrent owners; a newer lock always survives a stale unlock.

### Step 5: Make local lock caches atomic and concurrency-safe

Write cache updates through a unique temp file, sync, and atomic rename under a common-Git-dir lock or CAS-equivalent. Key cache scope by canonical remote identity, not only a user-provided name. A cache failure must not roll back an already successful remote lock transition; report it as repairable local state.

**Verify**: concurrent linked-worktree lock/list/unlock operations never produce invalid JSON or lose an unrelated record.

### Step 6: Add integrity repair and audit commands

Extend fsck/doctor output to distinguish trusted receipt, verified legacy object, stale receipt repaired, missing object, and corrupt object. Do not delete remote objects. Emit counts and remediation without secrets.

**Verify**: tests cover every state and stable machine-readable output.

## Test plan

- Golden/property-test receipt and lock schemas, including unknown versions and cross-prefix records.
- Use a counting/versioned fake store to inject crashes at object/receipt boundaries and validate legacy fallback/backfill.
- Race lock/unlock/force/relock/expiry operations with stale tokens and authenticated principals.
- Run concurrent Main worktree and Linked worktree cache updates and assert valid JSON plus preservation of unrelated records.

## Acceptance criteria

- [ ] A successful Crab upload creates a validator-bound verification receipt after durable bytes.
- [ ] Already-verified publication requires no object-body GET.
- [ ] Legacy/mismatched state falls back to streamed SHA-256 and safely backfills.
- [ ] No code treats ETag alone as an LFS OID.
- [ ] Stale unlock operations cannot delete or invalidate a newer lock.
- [ ] Equivalent Worktree path aliases map to one lock; outside-Worktree paths are rejected.
- [ ] Managed lock ownership and force authorization use authenticated principals.
- [ ] Linked-worktree lock-cache updates are atomic and preserve unrelated records.
- [ ] Receipt/lock persistent formats are versioned, documented, and golden-tested.

## STOP conditions

- No provider-neutral validator/generation can bind a receipt to current bytes.
- Receipt ordering can produce a trusted record before object durability.
- Lock safety requires provider-specific behavior with no conservative fallback.
- The layout conflicts with repository-wide GC ownership or active-active fencing; obtain an architecture decision before continuing.

## Maintenance notes

Receipts are an acceleration index, not the source of content truth. Loss should cost performance only. Reviewers must examine provider overwrite/version semantics and active-active behavior.
